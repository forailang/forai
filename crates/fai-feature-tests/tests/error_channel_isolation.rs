//! Regression: an uncaught error in one test case must not poison the next.
//!
//! The whole `fai test` run shares one wasm instance. An error thrown deep in
//! a called function propagates upward by setting the `__error_flag` global;
//! when it reaches the fatal test-case wrapper the codegen traps to report the
//! failure but leaves the flag set. If the runner does not clear the error
//! channel between cases, the next case's first post-call check reads the
//! stale flag and re-traps with the stale `__error_value` — turning one
//! genuine failure into a cascade that fails every following case, pure
//! functions included. This was brain's "shard corruptor": nondeterministic,
//! layout-dependent, and invisible to every heap/RC checker because nothing is
//! actually corrupted — only a global flag leaks.
//!
//! The trigger must be a throw from a *called* function (not a direct in-body
//! `throw`): a direct throw traps with the flag unset, so it never leaks. That
//! asymmetry is exactly why plain assertion failures never cascaded but a
//! deep `throw Error(...)` did.

use fai_feature_tests::fai_binary;
use std::fs;
use std::process::Command;

/// `boom` throws from its own frame; the first case calls it uncaught (so the
/// error reaches the case wrapper via the flag). The second case is pure and
/// must still pass.
const SRC: &str = "# Nested throw: because the throw is in a CALLED frame (`inner`), not the test
# body directly, it propagates upward by setting the error flag and reaches the
# fatal test-case wrapper with the flag still set. (A direct in-body throw sets
# only the error VALUE, not the flag, so it does not leak — which is why plain
# assertion failures never cascaded but a deep `throw` did.)
def inner
    @return Int
do
    throw Error('deep boom')
end

# One frame above the throw.
def boom
    @return Int
do
    inner()
end

# A pure value used by the victim case.
def pureFour
    @return Int
do
    4
end

test inner
    it 'throws when called directly'
        var threw = false
        try
            let _n = inner()
        catch _e
            threw = true
        end
        assert.isTrue(threw)
    end
end

test boom
    it 'a deep uncaught error fails only this case'
        let _n = boom()
        assert.isTrue(true)
    end
end

test pureFour
    it 'a later pure case is not poisoned by the previous failure'
        assert.equals(pureFour(), 4)
    end
end

def main
    @return Void
do
    let _n = pureFour()
    print('ok')
end
";

#[test]
fn uncaught_error_does_not_cascade_to_later_cases() {
    let dir = std::env::temp_dir().join("fai-error-channel-isolation");
    fs::create_dir_all(&dir).expect("create fixture dir");
    let path = dir.join("main.fai");
    fs::write(&path, SRC).expect("write fixture");

    let out = Command::new(fai_binary())
        .arg("test")
        .arg(&path)
        .output()
        .expect("spawn fai");
    let output =
        String::from_utf8_lossy(&out.stderr).into_owned() + &String::from_utf8_lossy(&out.stdout);

    // The one intentionally-failing case must be reported with its real error.
    assert!(
        output.contains("deep boom"),
        "the genuine failure should surface once:\n{output}"
    );
    // The `inner` catch-case and the pure `pureFour` case must PASS — proof the
    // stale flag was cleared between cases. Before the fix this cascaded to
    // "1 passed, 2 failed" (pureFour poisoned with the same "deep boom" error).
    assert!(
        output.contains("2 passed"),
        "later cases must not be poisoned by the prior failure (expected 2 passed):\n{output}"
    );
    assert!(
        !output.contains("2 failed") && !output.contains("3 failed"),
        "only the one genuinely-failing case should fail — no cascade:\n{output}"
    );
    // The pure case must never appear on a failure line.
    assert!(
        !output.contains("pureFour — a later pure case"),
        "the pure case was poisoned by the previous failure:\n{output}"
    );
}
