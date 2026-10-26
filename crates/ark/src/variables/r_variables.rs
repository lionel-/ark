//
// r_variables.rs
//
// Copyright (C) 2023-2026 by Posit Software, PBC
//
//

use amalthea::comm::comm_channel::CommMsg;
use amalthea::comm::variables_comm::ClipboardFormatFormat;
use amalthea::comm::variables_comm::FormattedVariable;
use amalthea::comm::variables_comm::InspectedVariable;
use amalthea::comm::variables_comm::QueryTableSummaryResult;
use amalthea::comm::variables_comm::RefreshParams;
use amalthea::comm::variables_comm::UpdateParams;
use amalthea::comm::variables_comm::Variable;
use amalthea::comm::variables_comm::VariableList;
use amalthea::comm::variables_comm::VariablesBackendReply;
use amalthea::comm::variables_comm::VariablesBackendRequest;
use amalthea::comm::variables_comm::VariablesFrontendEvent;
use anyhow::anyhow;
use harp::environment::Binding;
use harp::environment::Environment;
use harp::environment::EnvironmentFilter;
use harp::exec::RFunction;
use harp::exec::RFunctionExt;
use harp::get_option;
use harp::object::r_length;
use harp::object::RObject;
use harp::utils::r_assert_type;
use harp::utils::r_is_function;
use harp::vector::CharacterVector;
use harp::vector::Vector;
use harp::RSymbol;
use harp::R_ENVS;
use libr::R_GlobalEnv;
use libr::Rf_ScalarLogical;
use libr::ENVSXP;
use stdext::result::ResultExt;

use crate::comm_handler::handle_rpc_request;
use crate::comm_handler::CommHandler;
use crate::comm_handler::CommHandlerContext;
use crate::comm_handler::EnvironmentChanged;
use crate::console;
use crate::console::Console;
use crate::data_explorer::r_data_explorer::DataExplorerMode;
use crate::data_explorer::r_data_explorer::DataObjectEnvInfo;
use crate::data_explorer::r_data_explorer::RDataExplorer;
use crate::data_explorer::r_data_explorer::DATA_EXPLORER_COMM_NAME;
use crate::data_explorer::summary_stats::summary_stats;
use crate::object_explorer::r_object_explorer::path_accessor;
use crate::object_explorer::r_object_explorer::RObjectExplorer;
use crate::object_explorer::r_object_explorer::OBJECT_EXPLORER_COMM_NAME;
use crate::variables::variable::is_explorable;
use crate::variables::variable::is_viewable_character;
use crate::variables::variable::try_dispatch_view;
use crate::variables::variable::PositronVariable;
use crate::view::view;

/**
 * The R Variables handler provides the server side of Positron's Variables panel, and is
 * responsible for creating and updating the list of variables.
 */
pub struct RVariables {
    pub env: RObject,
    current_bindings: Vec<Binding>,
    version: u64,

    /// Whether we need to send an initial `Refresh` event to the frontend,
    /// which is required for frontend initialization. Set on construction,
    /// cleared after the first `update()` call.
    needs_initial_refresh: bool,
}

impl std::fmt::Debug for RVariables {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RVariables")
            .field("env", &self.env)
            .field("version", &self.version)
            .finish_non_exhaustive()
    }
}

impl RVariables {
    /// Create a variables handler on the R thread.
    pub fn new(env: RObject) -> Self {
        // Validate that the RObject we were passed is actually an environment
        if let Err(err) = r_assert_type(env.sexp, &[ENVSXP]) {
            log::warn!(
                "Variables: Attempt to monitor or list non-environment object {env:?} ({err:?})"
            );
        }

        Self {
            env,
            current_bindings: vec![],
            version: 0,
            needs_initial_refresh: true,
        }
    }

    fn update_bindings(&mut self, new_bindings: Vec<Binding>) -> u64 {
        self.current_bindings = new_bindings;
        self.version += 1;

        self.version
    }

    #[tracing::instrument(level = "trace", skip_all)]
    fn list_variables(&mut self) -> Vec<Variable> {
        let mut variables: Vec<Variable> = vec![];
        for binding in &self.current_bindings {
            variables.push(PositronVariable::new(binding).var());
        }

        variables
    }

    fn handle_rpc(
        &mut self,
        req: VariablesBackendRequest,
        ctx: &CommHandlerContext,
    ) -> anyhow::Result<VariablesBackendReply> {
        match req {
            VariablesBackendRequest::List => {
                let list = self.list_variables();
                let count = list.len() as i64;
                Ok(VariablesBackendReply::ListReply(VariableList {
                    variables: list,
                    length: count,
                    version: Some(self.version as i64),
                }))
            },
            VariablesBackendRequest::Clear(params) => {
                self.clear(params.include_hidden_objects)?;
                self.update(ctx);
                Ok(VariablesBackendReply::ClearReply())
            },
            VariablesBackendRequest::Delete(params) => {
                self.delete(params.names.clone())?;
                Ok(VariablesBackendReply::DeleteReply(params.names))
            },
            VariablesBackendRequest::Inspect(params) => {
                let children = self.inspect(&params.path)?;
                let count = children.len() as i64;
                Ok(VariablesBackendReply::InspectReply(InspectedVariable {
                    children,
                    length: count,
                }))
            },
            VariablesBackendRequest::ClipboardFormat(params) => {
                let content = self.clipboard_format(&params.path, params.format.clone())?;
                Ok(VariablesBackendReply::ClipboardFormatReply(
                    FormattedVariable { content },
                ))
            },
            VariablesBackendRequest::View(params) => {
                let viewer_id = self.view(&params.path)?;
                Ok(VariablesBackendReply::ViewReply(viewer_id))
            },
            VariablesBackendRequest::QueryTableSummary(params) => {
                let result = self.query_table_summary(&params.path, &params.query_types)?;
                Ok(VariablesBackendReply::QueryTableSummaryReply(result))
            },
        }
    }

    /**
     * Clear the environment. Uses rm(envir = <env>, list = ls(<env>, all.names = TRUE))
     */
    fn clear(&mut self, include_hidden_objects: bool) -> Result<(), harp::error::Error> {
        unsafe {
            let env = self.env.clone();

            let mut list = RFunction::new("base", "ls")
                .param("envir", *env)
                .param("all.names", Rf_ScalarLogical(include_hidden_objects as i32))
                .call()?;

            if *env == R_GlobalEnv {
                list = RFunction::new("base", "setdiff")
                    .add(list)
                    .add(RObject::from(".Random.seed"))
                    .call()?;
            }

            RFunction::new("base", "rm")
                .param("list", list)
                .param("envir", *env)
                .call()?;

            Ok(())
        }
    }

    /**
     * Clear the environment. Uses rm(envir = <env>, list = ls(<env>, all.names = TRUE))
     */
    fn delete(&mut self, variables: Vec<String>) -> Result<(), harp::error::Error> {
        let variables: Vec<&str> = variables.iter().map(|s| s as &str).collect();

        let env = self.env.clone();

        let result = RFunction::new("base", "rm")
            .param("list", CharacterVector::create(variables))
            .param("envir", env)
            .call();

        result?;
        Ok(())
    }

    fn clipboard_format(
        &mut self,
        path: &[String],
        format: ClipboardFormatFormat,
    ) -> anyhow::Result<String> {
        let env = self.env.clone();
        PositronVariable::clip(env, path, &format)
    }

    fn inspect(&mut self, path: &[String]) -> anyhow::Result<Vec<Variable>> {
        let env = self.env.clone();
        PositronVariable::inspect(env, path)
    }

    /// Open a data viewer for the given variable.
    ///
    /// - `path`: The path to the variable to view, as an array of access keys
    ///
    /// Returns the ID of the comm managing the view, if any.
    fn view(&mut self, path: &[String]) -> Result<Option<String>, harp::error::Error> {
        let env = self.env.clone();
        let obj = PositronVariable::resolve_data_object(env.clone(), path)?;

        // Try custom view method first (e.g., for connections)
        if try_dispatch_view(obj.sexp).map_err(harp::Error::Anyhow)? {
            return Ok(None);
        }

        // Functions and single strings open in an editor
        if r_is_function(obj.sexp) || (is_viewable_character(obj.sexp) && r_length(obj.sexp) == 1) {
            harp::as_result(view(&obj, path, &env))?;
            return Ok(None);
        }

        if is_explorable(obj.sexp) {
            return self.view_object(obj, path, env).map(Some);
        }

        let name = unsafe { path.get_unchecked(path.len() - 1) };

        let binding = DataObjectEnvInfo {
            name: name.to_string(),
            env,
        };

        let explorer = RDataExplorer::new(name.clone(), obj, Some(binding), DataExplorerMode::Full)
            .map_err(harp::Error::Anyhow)?;
        let viewer_id = Console::get_mut()
            .comm_open_backend(DATA_EXPLORER_COMM_NAME, Box::new(explorer))
            .map_err(harp::Error::Anyhow)?;
        Ok(Some(viewer_id))
    }

    /// Opens an object explorer on the object at `path`, watching the
    /// top-level variable it was read from for changes.
    fn view_object(
        &self,
        obj: RObject,
        path: &[String],
        env: RObject,
    ) -> Result<String, harp::error::Error> {
        let (name, path_in_binding) = match path.split_first() {
            Some(split) => split,
            None => return Err(harp::Error::Anyhow(anyhow!("Can't view an empty path"))),
        };
        let value = PositronVariable::resolve_data_object(env.clone(), std::slice::from_ref(name))?;
        let accessor = path_accessor(name, value, path_in_binding);
        let title = accessor.clone().unwrap_or_else(|| name.clone());
        let binding = DataObjectEnvInfo {
            name: name.clone(),
            env,
        };

        let explorer = RObjectExplorer::new(
            title,
            obj,
            accessor,
            Some((binding, path_in_binding.to_vec())),
            false,
        );
        Console::get_mut()
            .comm_open_backend(OBJECT_EXPLORER_COMM_NAME, Box::new(explorer))
            .map_err(harp::Error::Anyhow)
    }

    /// Query table summary for the given variable.
    ///
    /// - `path`: The path to the variable to summarize, as an array of access keys
    /// - `query_types`: A list of query types (e.g. "summary_stats")
    ///
    /// Returns summary information about the table including schemas and profiles.
    fn query_table_summary(
        &mut self,
        path: &[String],
        query_types: &[String],
    ) -> anyhow::Result<QueryTableSummaryResult> {
        let env = self.env.clone();
        let table = PositronVariable::resolve_data_object(env, path)?;

        let kind = if harp::utils::r_is_data_frame(table.sexp) {
            harp::TableKind::Dataframe
        } else if harp::utils::r_is_matrix(table.sexp) {
            harp::TableKind::Matrix
        } else {
            return Err(anyhow!(
                "Object is not a supported table type (data.frame or matrix)"
            ));
        };

        let num_cols = match kind {
            harp::TableKind::Dataframe => {
                let ncol = harp::DataFrame::n_col(table.sexp)?;
                ncol as i64
            },
            harp::TableKind::Matrix => {
                let (_nrow, ncol) = harp::Matrix::dim(table.sexp)?;
                ncol as i64
            },
        };

        let shapes = RDataExplorer::get_shape(table.clone())?;

        let column_schemas: Vec<String> = shapes
            .columns
            .iter()
            .map(serde_json::to_string)
            .collect::<Result<Vec<_>, _>>()?;

        let mut column_profiles: Vec<String> = vec![];

        if query_types.contains(&"summary_stats".to_string()) {
            let profiles: Vec<String> = shapes
                .columns
                .iter()
                .enumerate()
                .map(|(i, schema)| -> anyhow::Result<String> {
                    let column = harp::tbl_get_column(table.sexp, i as i32, kind)?;

                    let format_options = amalthea::comm::data_explorer_comm::FormatOptions {
                        large_num_digits: 4,
                        small_num_digits: 6,
                        max_integral_digits: 7,
                        max_value_length: 1000,
                        thousands_sep: None,
                    };

                    let summary_stats =
                        summary_stats(column.sexp, schema.type_display, &format_options).map(
                            |stats| serde_json::to_value(stats).unwrap_or(serde_json::Value::Null),
                        )?;

                    let profile = serde_json::json!({
                        "column_name": schema.column_name,
                        "type_display": format!("{:?}", schema.type_display).to_lowercase(),
                        "summary_stats": summary_stats,
                    })
                    .to_string();

                    Ok(profile)
                })
                .collect::<anyhow::Result<Vec<String>>>()?;

            column_profiles.extend(profiles);
        }

        Ok(QueryTableSummaryResult {
            num_rows: shapes.num_rows as i64,
            num_columns: num_cols,
            column_schemas,
            column_profiles,
        })
    }

    /// Determines whether or not we should be showing the special `.Last.value` object
    fn show_last_value(&self) -> bool {
        matches!(
            get_option("positron.show_last_value").get_bool(0),
            Ok(Some(true))
        )
    }

    fn last_value() -> harp::Result<Binding> {
        Binding::new(
            &Environment::view(R_ENVS.base),
            RSymbol::from(".Last.value"),
        )
    }

    #[tracing::instrument(level = "trace", skip_all)]
    fn update(&mut self, ctx: &CommHandlerContext) {
        let needs_initial_refresh = self.needs_initial_refresh;
        self.needs_initial_refresh = false;

        let env_changed = self.update_env();

        // Send a full refresh if the environment changed (e.g. entered
        // or exited debug mode) or on the first update after startup.
        if env_changed || needs_initial_refresh {
            self.update_bindings(self.bindings());

            let mut variables: Vec<Variable> = vec![];

            for binding in &self.current_bindings {
                variables.push(PositronVariable::new(binding).var());
            }

            let length = variables.len() as i64;
            ctx.send_event(&VariablesFrontendEvent::Refresh(RefreshParams {
                variables,
                length,
                version: self.version as i64,
            }));

            return;
        }

        // Incremental update: diff old vs new bindings
        let mut assigned: Vec<Variable> = vec![];
        let mut removed: Vec<String> = vec![];

        let new_bindings = self.bindings();

        let mut old_iter = self.current_bindings.iter();
        let mut old_next = old_iter.next();

        let mut new_iter = new_bindings.iter();
        let mut new_next = new_iter.next();

        loop {
            match (old_next, new_next) {
                // nothing more to do
                (None, None) => break,

                // No more old, collect last new into added
                (None, Some(mut new)) => {
                    loop {
                        assigned.push(PositronVariable::new(new).var());

                        match new_iter.next() {
                            Some(x) => {
                                new = x;
                            },
                            None => break,
                        };
                    }
                    break;
                },

                // No more new, collect the last old into removed
                (Some(mut old), None) => {
                    loop {
                        removed.push(old.name.to_string());

                        match old_iter.next() {
                            Some(x) => {
                                old = x;
                            },
                            None => break,
                        };
                    }

                    break;
                },

                (Some(old), Some(new)) => {
                    if old.name == new.name {
                        if old.value.id() != new.value.id() {
                            assigned.push(PositronVariable::new(new).var());
                        }
                        old_next = old_iter.next();
                        new_next = new_iter.next();
                    } else if old.name < new.name {
                        removed.push(old.name.to_string());
                        old_next = old_iter.next();
                    } else {
                        assigned.push(PositronVariable::new(new).var());
                        new_next = new_iter.next();
                    }
                },
            }
        }

        if !assigned.is_empty() || !removed.is_empty() {
            self.update_bindings(new_bindings);
            ctx.send_event(&VariablesFrontendEvent::Update(UpdateParams {
                assigned,
                removed,
                unevaluated: vec![],
                version: self.version as i64,
            }));
        }
    }

    /// Updates `self.env` to the current effective environment if it changed
    /// (e.g. entering or exiting debug mode). Returns `true` if switched.
    fn update_env(&mut self) -> bool {
        if !Console::is_initialized() {
            return false;
        }

        let env = console::selected_env();

        if env.sexp == self.env.sexp {
            return false;
        }

        self.env = env;
        true
    }

    fn bindings(&self) -> Vec<Binding> {
        let env = self.env.clone();
        let env = Environment::new_filtered(env, EnvironmentFilter::ExcludeHidden);

        let mut bindings: Vec<Binding> = env.iter().filter_map(|b| b.ok()).collect();

        if self.show_last_value() {
            if let Some(last_value) = Self::last_value().log_err() {
                bindings.push(last_value);
            }
        }

        bindings.sort_by_key(|a| a.name);

        bindings
    }
}

pub const VARIABLES_COMM_NAME: &str = "positron.variables";

impl CommHandler for RVariables {
    fn handle_open(&mut self, ctx: &CommHandlerContext) {
        self.update(ctx);
    }

    fn handle_msg(&mut self, msg: CommMsg, ctx: &CommHandlerContext) {
        handle_rpc_request(&ctx.outgoing_tx, VARIABLES_COMM_NAME, msg, |req| {
            self.handle_rpc(req, ctx)
        });
    }

    fn handle_environment(&mut self, _event: &EnvironmentChanged, ctx: &CommHandlerContext) {
        self.update(ctx);
    }
}
