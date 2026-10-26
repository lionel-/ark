use amalthea::comm::variables_comm::ClearParams;
use amalthea::comm::variables_comm::UpdateParams;
use amalthea::comm::variables_comm::VariablesBackendReply;
use amalthea::comm::variables_comm::VariablesBackendRequest;
use amalthea::comm::variables_comm::VariablesFrontendEvent;
use amalthea::comm::variables_comm::ViewParams;
use amalthea::fixtures::dummy_frontend::ExecuteRequestOptions;
use amalthea::wire::comm_close::CommClose;
use ark_test::DummyArkFrontend;

#[test]
fn test_variables_last_value() {
    let frontend = DummyArkFrontend::lock();

    // Set up a global variable before opening the variables comm
    frontend.execute_request_invisibly("test_var <- 'hello'");

    // Open the variables comm and receive the initial Refresh
    let (_, initial) = frontend.open_variables_comm();
    let names: Vec<&str> = initial
        .variables
        .iter()
        .map(|v| v.display_name.as_str())
        .collect();
    assert!(names.contains(&"test_var"));
    assert!(!names.contains(&".Last.value"));

    // Request that Ark start showing `.Last.value`
    let update = execute_with_update(&frontend, "options(positron.show_last_value = TRUE)");

    // The variables pane should have sent an Update, and now `.Last.value` should be in there
    let names: Vec<&str> = update
        .assigned
        .iter()
        .map(|v| v.display_name.as_str())
        .collect();
    assert!(!names.contains(&"test_var"));
    assert!(names.contains(&".Last.value"));

    // Set up another variable holding `NULL`
    let update = execute_with_update(&frontend, "test_var2 <- NULL");

    // Should get new variable and `.Last.value` again
    let names: Vec<&str> = update
        .assigned
        .iter()
        .map(|v| v.display_name.as_str())
        .collect();
    assert!(!names.contains(&"test_var"));
    assert!(names.contains(&"test_var2"));
    assert!(names.contains(&".Last.value"));

    // Set up another variable holding `NULL` a second time in a row
    let update = execute_with_update(&frontend, "test_var3 <- NULL");

    // Should get new variable but NOT `.Last.value` again, it didn't change! Same pointer!
    let names: Vec<&str> = update
        .assigned
        .iter()
        .map(|v| v.display_name.as_str())
        .collect();
    assert!(!names.contains(&"test_var"));
    assert!(!names.contains(&"test_var2"));
    assert!(names.contains(&"test_var3"));
    assert!(!names.contains(&".Last.value"));

    // Request that Ark stop showing `.Last.value`
    let update = execute_with_update(&frontend, "options(positron.show_last_value = FALSE)");

    // `.Last.value` should get removed
    assert!(update.removed.contains(&String::from(".Last.value")));
}

// https://github.com/posit-dev/positron/issues/13294
#[test]
fn test_readr_read_csv_does_not_crash_variables_pane() {
    let frontend = DummyArkFrontend::lock();

    if !frontend.is_installed("readr") {
        println!("Skipping test: readr package not installed");
        return;
    }

    let csv_path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/data/test-issue-13294.csv"
    )
    .replace('\\', "/");

    let (_, initial) = frontend.open_variables_comm();
    assert!(initial.variables.is_empty());

    // This triggers the variables pane to compute `r_size()` on the tibble
    // produced by readr, which has deeply nested ALTREP internals.
    let code = format!("x <- readr::read_csv('{csv_path}', show_col_types = FALSE)");
    let update = execute_with_update(&frontend, &code);
    assert!(update.assigned.iter().any(|v| v.display_name == "x"));

    // Exercise `r_size` directly via `.ps.internal(obj_size())`
    frontend.execute_request(".ps.internal(obj_size(x))", |_result| {});
}

#[test]
fn test_variables_rpc_finishes_before_next_execution() {
    let frontend = DummyArkFrontend::lock();
    frontend.execute_request_invisibly("x <- 1; .hidden <- 2; set.seed(42)");
    let (comm_id, initial) = frontend.open_variables_comm();
    assert_eq!(initial.version, 1);

    let request = VariablesBackendRequest::Clear(ClearParams {
        include_hidden_objects: false,
    });
    let mut data = serde_json::to_value(request).unwrap();
    data["id"] = serde_json::json!("clear");
    frontend.send_shell_comm_msg(comm_id.clone(), data);
    // Queue execution without waiting for the RPC. Its Busy must follow the
    // variables update, RPC reply, and Idle of the comm request.
    frontend.send_execute_request(
        "stopifnot(!exists('x'), exists('.hidden'), exists('.Random.seed'))",
        ExecuteRequestOptions::default(),
    );

    frontend.recv_iopub_busy();
    let event = frontend.recv_iopub_comm_msg();
    assert_eq!(event.comm_id, comm_id);
    let VariablesFrontendEvent::Update(update) = serde_json::from_value(event.data).unwrap() else {
        panic!("Expected variables update");
    };
    assert!(update.assigned.is_empty());
    assert_eq!(update.removed, vec!["x"]);
    assert_eq!(update.version, 2);
    let reply = frontend.recv_iopub_comm_msg();
    assert_eq!(reply.comm_id, comm_id);
    assert_eq!(
        serde_json::from_value::<VariablesBackendReply>(reply.data).unwrap(),
        VariablesBackendReply::ClearReply()
    );
    frontend.recv_iopub_idle();

    frontend.recv_iopub_busy();
    frontend.recv_iopub_execute_input();
    frontend.recv_iopub_idle();
    frontend.recv_shell_execute_reply();

    frontend.send_shell(CommClose { comm_id });
    frontend.recv_iopub_busy();
    frontend.recv_iopub_idle();
    // Closing the comm must remove its environment-change handler too.
    frontend.execute_request_invisibly("x <- 3");
}

#[test]
fn test_variables_view_opens_explorers_before_reply() {
    let frontend = DummyArkFrontend::lock();
    frontend.execute_request_invisibly("df <- data.frame(x = 1:3); obj <- list(a = 1)");
    let (comm_id, initial) = frontend.open_variables_comm();
    assert_eq!(initial.version, 1);

    for (name, target) in [
        ("df", "positron.dataExplorer"),
        ("obj", "positron.objectExplorer"),
    ] {
        let request = VariablesBackendRequest::View(ViewParams {
            path: vec![String::from(name)],
        });
        let mut data = serde_json::to_value(request).unwrap();
        data["id"] = serde_json::json!("view");
        frontend.send_shell_comm_msg(comm_id.clone(), data);
        frontend.recv_iopub_busy();
        let opened = frontend.recv_iopub_comm_open();
        assert_eq!(opened.target_name, target);
        let reply = frontend.recv_iopub_comm_msg();
        assert_eq!(reply.comm_id, comm_id);
        assert_eq!(
            serde_json::from_value::<VariablesBackendReply>(reply.data).unwrap(),
            VariablesBackendReply::ViewReply(Some(opened.comm_id))
        );
        frontend.recv_iopub_idle();
    }
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
