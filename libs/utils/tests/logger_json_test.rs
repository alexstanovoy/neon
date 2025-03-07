// This could be in ../src/logging.rs but since the logger is global, these
// can't be run in threads of the same process
use std::fs::File;
use std::io::{BufRead, BufReader, Lines};
use tracing::*;
use utils::test_init_file_logger;

fn read_lines(file: File) -> Lines<BufReader<File>> {
    BufReader::new(file).lines()
}

#[test]
fn test_json_format_has_message_and_custom_field() {
    std::env::set_var("RUST_LOG", "info");

    let log_file = test_init_file_logger!("info", "json");

    let custom_field: &str = "hi";
    trace!(custom = %custom_field, "test log message");
    debug!(custom = %custom_field, "test log message");
    info!(custom = %custom_field, "test log message");
    warn!(custom = %custom_field, "test log message");
    error!(custom = %custom_field, "test log message");

    let lines = read_lines(log_file);
    for line in lines {
        let content = line.unwrap();
        let json_object = serde_json::from_str::<serde_json::Value>(&content).unwrap();

        assert_eq!(json_object["fields"]["custom"], "hi");
        assert_eq!(json_object["fields"]["message"], "test log message");

        assert_ne!(json_object["level"], "TRACE");
        assert_ne!(json_object["level"], "DEBUG");
    }
}
