#[test]
fn watch_quota_table_does_not_silently_cap_rows_at_six() {
    let src = include_str!("../src/tui.rs");
    let quota_panel = src
        .split("// --- quotas:")
        .nth(1)
        .and_then(|section| section.split("// --- balances").next())
        .expect("quota panel source section");

    assert!(
        !src.contains("d.quotas.len().min(6)"),
        "watch panel height must account for every quota row"
    );
    assert!(
        !quota_panel.contains(".take(6)"),
        "watch rendering must not silently discard quota rows after the sixth"
    );
}
