// Keep this in a separate test binary: path overrides are process-global OnceLocks.
#[test]
fn custom_data_directory_isolates_user_files() {
    let directory = tempfile::tempdir().unwrap();
    let root = paths::set_custom_data_dir(directory.path().to_str().unwrap());

    assert_eq!(paths::data_dir(), root);
    assert_eq!(*paths::config_dir(), root.join("config"));
    assert_eq!(*paths::settings_file(), root.join("config/settings.json"));
    assert_eq!(*paths::state_dir(), root.join("state"));
    assert_eq!(*paths::temp_dir(), root.join("cache"));
    assert_eq!(*paths::logs_dir(), root.join("logs"));
    assert_eq!(*paths::log_file(), root.join("logs/Zed.log"));
    assert_eq!(*paths::old_log_file(), root.join("logs/Zed.log.old"));
    assert_eq!(*paths::database_dir(), root.join("db"));
    assert_eq!(*paths::hang_traces_dir(), root.join("hang_traces"));
}
