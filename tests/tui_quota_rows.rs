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

#[test]
fn watch_local_refresh_preserves_every_local_usage_stream() {
    let src = include_str!("../src/tui.rs");
    let local_tick = src
        .split("} else if !is_paused && last_local.elapsed() >= locald {")
        .nth(1)
        .and_then(|section| section.split("std::thread::sleep").next())
        .expect("local refresh branch");

    assert!(
        local_tick.contains("local::claude_code::collect"),
        "the local tick must refresh Claude Code transcripts"
    );
    assert!(
        local_tick.contains("local::opencode::collect"),
        "the local tick must refresh OpenCode local usage directly"
    );
    for provider in ["codex::Codex", "gemini::Gemini", "qwen::Qwen"] {
        assert!(
            local_tick.contains(provider),
            "the local tick must refresh the {provider} local ledger"
        );
    }
    for provider in [
        "anthropic::Anthropic",
        "claude_sub::ClaudeSub",
        "deepseek::DeepSeek",
        "glm::Glm",
        "kimi::Kimi",
        "openai::OpenAi",
    ] {
        assert!(
            !local_tick.contains(provider),
            "the local tick must never call the network-backed {provider} provider"
        );
    }
    assert!(
        !local_tick.contains("Some(&filt)"),
        "a provider filter is CLI report semantics, not local-source selection"
    );
    assert!(
        !local_tick.contains("crate::gather"),
        "the local tick must not invoke the general provider gather path"
    );
    assert!(
        local_tick.contains("apply_local_refresh("),
        "the local tick must preserve API rows while replacing local rows"
    );
}

#[test]
fn watch_local_tick_downgrades_completion_for_opencode_db_failure_before_apply() {
    let src = include_str!("../src/tui.rs");
    let local_tick = src
        .split("} else if !is_paused && last_local.elapsed() >= locald {")
        .nth(1)
        .and_then(|section| section.split("std::thread::sleep").next())
        .expect("local refresh branch");

    let opencode_site = local_tick
        .find("local::opencode::collect")
        .expect("the local tick must collect OpenCode usage directly");
    let failure_arm = local_tick
        .find("opencode_db_failed(")
        .expect("the local tick must consult the OpenCode DB-failure predicate");
    let apply = local_tick
        .find("apply_local_refresh(")
        .expect("the local tick must apply the refresh state");

    assert!(
        opencode_site < failure_arm && failure_arm < apply,
        "OpenCode DB failure must downgrade completion before apply_local_refresh"
    );
    assert!(
        !local_tick.contains("available"),
        "the local tick must not run an availability precursor probe"
    );
    assert!(
        local_tick.contains("opencode_db_failed(&opencode.notes)"),
        "the predicate must classify the OpenCode collector's own notes"
    );
}
