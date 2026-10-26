//
// variables.rs
//
// Copyright (C) 2023-2026 Posit Software, PBC. All rights reserved.
//
//

use amalthea::comm::variables_comm::ClearParams;
use amalthea::comm::variables_comm::DeleteParams;
use amalthea::comm::variables_comm::QueryTableSummaryParams;
use amalthea::comm::variables_comm::UpdateParams;
use amalthea::comm::variables_comm::VariablesBackendReply;
use amalthea::comm::variables_comm::VariablesBackendRequest;
use amalthea::fixtures::dummy_frontend::ExecuteRequestOptions;
use amalthea::wire::jupyter_message::Message;
use ark_test::DummyArkFrontend;

#[test]
fn test_variables_list() {
    let frontend = DummyArkFrontend::lock();
    let (comm_id, initial) = frontend.open_variables_comm();
    assert!(initial.variables.is_empty());
    assert_eq!(initial.version, 1);

    let update = execute_with_update(&frontend, "everything <- 42L");
    assert_eq!(update.assigned.len(), 1);
    assert_eq!(update.assigned[0].display_name, "everything");
    assert!(update.removed.is_empty());
    assert_eq!(update.version, 2);

    let data = rpc(&frontend, &comm_id, VariablesBackendRequest::List);
    let VariablesBackendReply::ListReply(list) = serde_json::from_value(data).unwrap() else {
        panic!("Expected list reply");
    };
    assert_eq!(list.variables.len(), 1);
    assert_eq!(list.variables[0].display_name, "everything");
    assert_eq!(list.version, Some(2));

    let update = execute_with_update(&frontend, "nothing <- 43L; rm(everything)");
    assert_eq!(update.assigned.len(), 1);
    assert_eq!(update.assigned[0].display_name, "nothing");
    assert_eq!(update.removed, ["everything"]);
    assert_eq!(update.version, 3);

    // Clear emits an update before its RPC reply, all within Busy/Idle.
    let mut data = serde_json::to_value(VariablesBackendRequest::Clear(ClearParams {
        include_hidden_objects: true,
    }))
    .unwrap();
    data["id"] = serde_json::json!("clear");
    let request_id = frontend.send_shell_comm_msg(comm_id.clone(), data);
    frontend.recv_iopub_busy();
    let update = frontend.recv_variables_update();
    assert!(update.assigned.is_empty());
    assert_eq!(update.removed, ["nothing"]);
    assert_eq!(update.version, 4);
    let Message::CommMsg(reply) = frontend.recv_iopub() else {
        panic!("Expected RPC reply");
    };
    assert_eq!(reply.content.comm_id, comm_id);
    assert_eq!(reply.parent_header.unwrap().msg_id, request_id);
    assert_eq!(
        serde_json::from_value::<VariablesBackendReply>(reply.content.data).unwrap(),
        VariablesBackendReply::ClearReply()
    );
    frontend.recv_iopub_idle();
    frontend.execute_request_invisibly("stopifnot(length(ls()) == 0L)");

    let update = execute_with_update(&frontend, "a <- 42L; b <- 43L");
    assert_eq!(update.assigned.len(), 2);
    assert_eq!(update.assigned[0].display_name, "a");
    assert_eq!(update.assigned[1].display_name, "b");
    assert!(update.removed.is_empty());
    assert_eq!(update.version, 5);

    let data = rpc(
        &frontend,
        &comm_id,
        VariablesBackendRequest::Delete(DeleteParams {
            names: vec![String::from("a")],
        }),
    );
    assert_eq!(
        serde_json::from_value::<VariablesBackendReply>(data).unwrap(),
        VariablesBackendReply::DeleteReply(vec![String::from("a")])
    );
    let update = execute_with_update(&frontend, "stopifnot(!exists('a'), b == 43L)");
    assert!(update.assigned.is_empty());
    assert_eq!(update.removed, ["a"]);
    assert_eq!(update.version, 6);
}

#[test]
fn test_query_table_summary() {
    let frontend = DummyArkFrontend::lock();
    let (comm_id, initial) = frontend.open_variables_comm();
    assert!(initial.variables.is_empty());
    assert_eq!(initial.version, 1);

    let update = execute_with_update(
        &frontend,
        r#"
        test_df <- data.frame(
            numeric_col = c(1.5, 2.5, 3.5, NA),
            integer_col = c(1L, 2L, 3L, 4L),
            character_col = c('a', 'b', 'c', ''),
            logical_col = c(TRUE, FALSE, TRUE, NA),
            stringsAsFactors = FALSE
        )
        test_matrix <- matrix(
            1:12,
            nrow = 4,
            ncol = 3,
            dimnames = list(
                c('row1', 'row2', 'row3', 'row4'),
                c('col1', 'col2', 'col3')
            )
        )
        "#,
    );
    assert_eq!(update.assigned.len(), 2);
    assert_eq!(update.assigned[0].display_name, "test_df");
    assert_eq!(update.assigned[1].display_name, "test_matrix");
    assert_eq!(update.version, 2);

    let data = rpc(
        &frontend,
        &comm_id,
        VariablesBackendRequest::QueryTableSummary(QueryTableSummaryParams {
            path: vec![String::from("test_df")],
            query_types: vec![String::from("summary_stats")],
        }),
    );
    let VariablesBackendReply::QueryTableSummaryReply(result) =
        serde_json::from_value(data).unwrap()
    else {
        panic!("Expected table summary reply");
    };
    assert_eq!(result.num_rows, 4);
    assert_eq!(result.num_columns, 4);
    assert_eq!(result.column_schemas.len(), 4);
    assert_eq!(result.column_profiles.len(), 4);

    let schemas: Vec<serde_json::Value> = result
        .column_schemas
        .iter()
        .map(|schema| serde_json::from_str(schema).unwrap())
        .collect();
    assert_eq!(schemas[0]["column_name"], "numeric_col");
    assert_eq!(schemas[1]["column_name"], "integer_col");
    assert_eq!(schemas[2]["column_name"], "character_col");
    assert_eq!(schemas[3]["column_name"], "logical_col");
    assert_eq!(schemas[0]["type_display"], "floating");
    assert_eq!(schemas[1]["type_display"], "integer");
    assert_eq!(schemas[2]["type_display"], "string");
    assert_eq!(schemas[3]["type_display"], "boolean");

    let profiles: Vec<serde_json::Value> = result
        .column_profiles
        .iter()
        .map(|profile| serde_json::from_str(profile).unwrap())
        .collect();
    for profile in &profiles {
        assert!(profile["summary_stats"].is_object());
    }
    assert!(profiles[0]["summary_stats"]["number_stats"].is_object());
    assert!(profiles[3]["summary_stats"]["boolean_stats"].is_object());

    // Empty query_types requests schemas without profiles.
    let data = rpc(
        &frontend,
        &comm_id,
        VariablesBackendRequest::QueryTableSummary(QueryTableSummaryParams {
            path: vec![String::from("test_matrix")],
            query_types: vec![],
        }),
    );
    let VariablesBackendReply::QueryTableSummaryReply(result) =
        serde_json::from_value(data).unwrap()
    else {
        panic!("Expected table summary reply");
    };
    assert_eq!(result.num_rows, 4);
    assert_eq!(result.num_columns, 3);
    assert_eq!(result.column_schemas.len(), 3);
    assert!(result.column_profiles.is_empty());

    let schemas: Vec<serde_json::Value> = result
        .column_schemas
        .iter()
        .map(|schema| serde_json::from_str(schema).unwrap())
        .collect();
    assert_eq!(schemas[0]["column_name"], "col1");
    assert_eq!(schemas[1]["column_name"], "col2");
    assert_eq!(schemas[2]["column_name"], "col3");
    for schema in &schemas {
        assert_eq!(schema["type_display"], "integer");
    }

    let update = execute_with_update(&frontend, "non_table_obj <- list(a = 1, b = 2)");
    assert_eq!(update.assigned.len(), 1);
    assert_eq!(update.assigned[0].display_name, "non_table_obj");
    assert_eq!(update.version, 3);

    let data = rpc(
        &frontend,
        &comm_id,
        VariablesBackendRequest::QueryTableSummary(QueryTableSummaryParams {
            path: vec![String::from("non_table_obj")],
            query_types: vec![String::from("summary_stats")],
        }),
    );
    assert!(data["error"]["message"]
        .as_str()
        .unwrap()
        .contains("not a supported table type"));
}

fn execute_with_update(frontend: &DummyArkFrontend, code: &str) -> UpdateParams {
    frontend.send_execute_request(code, ExecuteRequestOptions::default());
    frontend.recv_iopub_busy();
    frontend.recv_iopub_execute_input();
    let update = frontend.recv_variables_update();
    frontend.recv_iopub_idle();
    frontend.recv_shell_execute_reply();
    update
}

fn rpc(
    frontend: &DummyArkFrontend,
    comm_id: &str,
    request: VariablesBackendRequest,
) -> serde_json::Value {
    let mut data = serde_json::to_value(request).unwrap();
    data["id"] = serde_json::json!("variables-rpc");
    let request_id = frontend.send_shell_comm_msg(String::from(comm_id), data);
    frontend.recv_iopub_busy();
    let Message::CommMsg(reply) = frontend.recv_iopub() else {
        panic!("Expected RPC reply");
    };
    assert_eq!(reply.content.comm_id, comm_id);
    assert_eq!(reply.parent_header.unwrap().msg_id, request_id);
    frontend.recv_iopub_idle();
    reply.content.data
}
