use varynth::sandbox::{Jail, SandboxMode};

#[test]
fn git_is_allowed_format_is_not() {
    let jail = Jail::new(
        std::env::current_dir().unwrap(),
        vec![],
        SandboxMode::WorkspaceWrite,
        vec!["git".into()],
    );
    assert!(jail.assert_shell("git status").is_ok());
    assert!(jail.assert_shell("format C:").is_err());
}

#[test]
fn read_only_blocks_writes() {
    let jail = Jail::new(
        std::env::current_dir().unwrap(),
        vec![],
        SandboxMode::ReadOnly,
        vec![],
    );
    assert!(jail.assert_write("x.txt").is_err());
}
