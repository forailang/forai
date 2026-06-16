use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap};
use std::rc::Rc;

use fai_compiler::ownership_abi::{OwnershipAux, OwnershipOp};

use super::debug_table::DbgTable;
use super::nan_box::{ADDR_MASK, QNAN, SIGN_BIT};

const HISTORY_LIMIT: usize = 16;

#[derive(Debug, Clone)]
struct OwnershipEvent {
    op: OwnershipOp,
    site: u32,
    value: i64,
    addr: Option<u32>,
    aux: i32,
}

#[derive(Debug, Clone)]
struct ProofFailure {
    kind: &'static str,
    event: OwnershipEvent,
}

#[derive(Default)]
struct OwnershipLedger {
    enabled: bool,
    dbg: Option<Rc<DbgTable>>,
    proof_seed: Option<OwnershipOp>,
    events: Vec<OwnershipEvent>,
    history: HashMap<u32, Vec<OwnershipEvent>>,
    credits: HashMap<u32, i32>,
    unmatched: Vec<OwnershipEvent>,
    proof_failures: Vec<ProofFailure>,
    saw_lifecycle_events: bool,
}

thread_local! {
    static LEDGER: RefCell<OwnershipLedger> = RefCell::new(OwnershipLedger::default());
}

pub(crate) fn reset(enabled: bool, dbg: Option<Rc<DbgTable>>) {
    LEDGER.with(|ledger| {
        *ledger.borrow_mut() = OwnershipLedger {
            enabled,
            dbg,
            proof_seed: ownership_seed_from_env(),
            ..OwnershipLedger::default()
        };
    });
}

pub(crate) fn is_enabled() -> bool {
    LEDGER.with(|ledger| ledger.borrow().enabled)
}

pub(crate) fn record_event(op_id: i32, site: i32, value: i64, aux: i32) {
    if !is_enabled() {
        return;
    }
    let Some(op) = OwnershipOp::from_id(op_id as u32) else {
        return;
    };
    let addr = object_addr(value);
    LEDGER.with(|ledger| {
        let mut ledger = ledger.borrow_mut();
        let event = OwnershipEvent {
            op,
            site: site as u32,
            value,
            addr,
            aux,
        };
        if let Some(addr) = addr {
            let history = ledger.history.entry(addr).or_default();
            history.push(event.clone());
            if history.len() > HISTORY_LIMIT {
                history.remove(0);
            }
            let delta = match op {
                OwnershipOp::Retain | OwnershipOp::Transfer => 1,
                OwnershipOp::Release | OwnershipOp::Cleanup | OwnershipOp::Discard => -1,
                OwnershipOp::Borrow
                | OwnershipOp::Store
                | OwnershipOp::Overwrite
                | OwnershipOp::Return
                | OwnershipOp::CallArgument => 0,
            };
            if delta != 0 {
                let credits = ledger.credits.entry(addr).or_insert(0);
                if delta > 0 {
                    *credits += delta;
                } else if *credits > 0 {
                    *credits += delta;
                } else if allows_untracked_consume(op) {
                    // Fresh owned expression disposal and first release of
                    // pre-helper container contents have no prior helper
                    // credit. Heap addresses are reused aggressively, so
                    // untracked discard/release events cannot prove duplicate
                    // disposal without allocation-event correlation.
                } else {
                    ledger.unmatched.push(event.clone());
                }
            }
            if ledger.proof_seed == Some(OwnershipOp::Transfer)
                && matches!(op, OwnershipOp::Store | OwnershipOp::Overwrite)
                && ledger.credits.get(&addr).copied().unwrap_or(0) <= 0
            {
                ledger.proof_failures.push(ProofFailure {
                    kind: "uncredited owning store; missing retain/transfer before store",
                    event: event.clone(),
                });
            }
        }
        ledger.events.push(event);
    });
}

pub(crate) fn record_alloc(addr: u32) {
    if !is_enabled() {
        return;
    }
    LEDGER.with(|ledger| {
        let mut ledger = ledger.borrow_mut();
        ledger.saw_lifecycle_events = true;
        ledger.credits.remove(&addr);
        ledger.history.remove(&addr);
    });
}

pub(crate) fn record_free(addr: u32) {
    if !is_enabled() {
        return;
    }
    LEDGER.with(|ledger| {
        let mut ledger = ledger.borrow_mut();
        ledger.saw_lifecycle_events = true;
        if ledger.proof_seed == Some(OwnershipOp::Cleanup) {
            if let Some(credits) = ledger.credits.get(&addr).copied() {
                if credits > 0 {
                    let event = ledger
                        .history
                        .get(&addr)
                        .and_then(|history| history.last())
                        .cloned()
                        .unwrap_or_else(|| OwnershipEvent {
                            op: OwnershipOp::Cleanup,
                            site: 0,
                            value: obj_value(addr),
                            addr: Some(addr),
                            aux: 0,
                        });
                    ledger.proof_failures.push(ProofFailure {
                        kind: "live helper credit retired by free; missing cleanup before free",
                        event,
                    });
                }
            }
        }
        ledger.credits.remove(&addr);
        ledger.history.remove(&addr);
    });
}

pub(crate) fn has_imbalance() -> bool {
    LEDGER.with(|ledger| {
        let ledger = ledger.borrow();
        ledger.enabled
            && (!ledger.unmatched.is_empty()
                || !ledger.proof_failures.is_empty()
                || (!ledger.saw_lifecycle_events
                    && ledger.credits.values().any(|credits| *credits != 0)))
    })
}

pub(crate) fn render_report() -> Option<String> {
    LEDGER.with(|ledger| {
        let ledger = ledger.borrow();
        if !ledger.enabled {
            return None;
        }
        let live_imbalanced: Vec<(u32, i32)> = if ledger.saw_lifecycle_events {
            Vec::new()
        } else {
            ledger
                .credits
                .iter()
                .filter_map(|(&addr, &credits)| (credits != 0).then_some((addr, credits)))
                .collect()
        };
        let imbalance_count =
            live_imbalanced.len() + ledger.unmatched.len() + ledger.proof_failures.len();
        let mut out = format!(
            "[ownership-check] {} event(s), {} object(s) with helper imbalance",
            ledger.events.len(),
            imbalance_count
        );
        if imbalance_count > 0 {
            let groups = ownership_groups(&ledger, &live_imbalanced);
            if !groups.is_empty() {
                out.push_str("\n  groups:");
                for group in groups.iter().take(8) {
                    out.push_str("\n    ");
                    out.push_str(group);
                }
            }
        }
        for (addr, credits) in live_imbalanced.iter().take(8) {
            out.push_str(&format!("\n  0x{addr:x}: helper credits {credits:+}"));
            if let Some(history) = ledger.history.get(addr) {
                out.push_str("\n    history:");
                for event in history.iter().take(HISTORY_LIMIT) {
                    out.push_str("\n      ");
                    out.push_str(&format_event(event, ledger.dbg.as_deref()));
                }
            }
        }
        for event in ledger
            .unmatched
            .iter()
            .take(8usize.saturating_sub(live_imbalanced.len()))
        {
            out.push_str("\n  unmatched ");
            out.push_str(&format_event(event, ledger.dbg.as_deref()));
            if let Some(addr) = event.addr {
                if let Some(history) = ledger.history.get(&addr) {
                    out.push_str("\n    history:");
                    for event in history.iter().take(HISTORY_LIMIT) {
                        out.push_str("\n      ");
                        out.push_str(&format_event(event, ledger.dbg.as_deref()));
                    }
                }
            }
        }
        for failure in ledger
            .proof_failures
            .iter()
            .take(8usize.saturating_sub(live_imbalanced.len() + ledger.unmatched.len()))
        {
            out.push_str("\n  proof ");
            out.push_str(failure.kind);
            out.push_str(" at ");
            out.push_str(&format_event(&failure.event, ledger.dbg.as_deref()));
        }
        Some(out)
    })
}

fn ownership_groups(ledger: &OwnershipLedger, live_imbalanced: &[(u32, i32)]) -> Vec<String> {
    let mut groups: BTreeMap<String, (usize, Option<u32>)> = BTreeMap::new();
    for (addr, credits) in live_imbalanced {
        let label = ledger
            .history
            .get(addr)
            .and_then(|history| history.last())
            .map(|event| format_event_group(event, ledger.dbg.as_deref()))
            .unwrap_or_else(|| "unknown ownership site".to_string());
        let key = format!("live helper credits {credits:+} at {label}");
        let entry = groups.entry(key).or_insert((0, Some(*addr)));
        entry.0 += 1;
    }
    for event in &ledger.unmatched {
        let key = format!(
            "unmatched {} at {}",
            event.op.name(),
            format_event_group(event, ledger.dbg.as_deref())
        );
        let entry = groups.entry(key).or_insert((0, event.addr));
        entry.0 += 1;
    }
    for failure in &ledger.proof_failures {
        let key = format!(
            "proof {} at {}",
            failure.kind,
            format_event_group(&failure.event, ledger.dbg.as_deref())
        );
        let entry = groups.entry(key).or_insert((0, failure.event.addr));
        entry.0 += 1;
    }
    groups
        .into_iter()
        .map(|(key, (count, sample_addr))| match sample_addr {
            Some(addr) => format!("{count} x {key} (sample 0x{addr:x})"),
            None => format!("{count} x {key}"),
        })
        .collect()
}

fn format_event_group(event: &OwnershipEvent, dbg: Option<&DbgTable>) -> String {
    let label = dbg
        .map(|dbg| dbg.ownership_site_label(event.site))
        .unwrap_or_else(|| format!("site={}", event.site));
    let aux = format_aux(event.aux);
    format!("{} {} {}", event.op.name(), label, aux)
}

fn format_event(event: &OwnershipEvent, dbg: Option<&DbgTable>) -> String {
    let label = dbg
        .map(|dbg| dbg.ownership_site_label(event.site))
        .unwrap_or_else(|| format!("site={}", event.site));
    let aux = format_aux(event.aux);
    format!(
        "{} {} {} value=0x{:x}",
        event.op.name(),
        label,
        aux,
        event.value as u64
    )
}

fn format_aux(aux: i32) -> String {
    match OwnershipAux::decode(aux) {
        Some((OwnershipAux::None, 0)) => "aux=none".to_string(),
        Some((kind, detail)) => format!("aux={:?}:{}", kind, detail),
        None => format!("aux={}", aux),
    }
}

fn allows_untracked_consume(op: OwnershipOp) -> bool {
    matches!(op, OwnershipOp::Discard | OwnershipOp::Release)
}

fn ownership_seed_from_env() -> Option<OwnershipOp> {
    let value = std::env::var("FAI_OWNERSHIP_SEED").ok()?;
    let name = value.strip_prefix("suppress-")?;
    OwnershipOp::ALL.into_iter().find(|op| op.name() == name)
}

fn object_addr(value: i64) -> Option<u32> {
    let bits = value as u64;
    ((bits & (QNAN | SIGN_BIT)) == (QNAN | SIGN_BIT)).then_some((bits & ADDR_MASK) as u32)
}

fn obj_value(addr: u32) -> i64 {
    (QNAN | SIGN_BIT | addr as u64) as i64
}

#[cfg(test)]
mod tests {
    use super::*;

    fn encode_u32_leb(mut value: u32, out: &mut Vec<u8>) {
        loop {
            let mut byte = (value & 0x7f) as u8;
            value >>= 7;
            if value != 0 {
                byte |= 0x80;
            }
            out.push(byte);
            if value == 0 {
                break;
            }
        }
    }

    fn dbg_with_site() -> Rc<DbgTable> {
        let json = "{\"version\":1,\"ownership_sites\":[{\"id\":7,\"op\":\"retain\",\"helper\":\"direct\",\"reason\":\"retain borrowed value\",\"file\":\"app.fai\",\"line\":4}],\"functions\":[]}";
        let mut wasm = vec![
            0x00, 0x61, 0x73, 0x6d, // magic
            0x01, 0x00, 0x00, 0x00, // version
        ];
        let mut payload = Vec::new();
        encode_u32_leb("fai-dbg".len() as u32, &mut payload);
        payload.extend_from_slice(b"fai-dbg");
        payload.extend_from_slice(json.as_bytes());
        wasm.push(0);
        encode_u32_leb(payload.len() as u32, &mut wasm);
        wasm.extend(payload);
        Rc::new(DbgTable::from_wasm(&wasm))
    }

    fn obj(addr: u32) -> i64 {
        obj_value(addr)
    }

    fn reset_seeded(seed: OwnershipOp) {
        LEDGER.with(|ledger| {
            *ledger.borrow_mut() = OwnershipLedger {
                enabled: true,
                proof_seed: Some(seed),
                ..OwnershipLedger::default()
            };
        });
    }

    #[test]
    fn report_names_imbalanced_helper_credit() {
        reset(true, Some(dbg_with_site()));
        record_event(OwnershipOp::Retain.id() as i32, 7, obj(0x100), 0);

        let report = render_report().expect("report");
        assert!(report.contains("1 event(s)"), "{report}");
        assert!(report.contains("0x100"), "{report}");
        assert!(report.contains("helper credits +1"), "{report}");
        assert!(
            report.contains("direct:retain:retain borrowed value (app.fai:4)"),
            "{report}"
        );
        assert!(report.contains("groups:"), "{report}");
        assert!(
            report.contains(
                "1 x live helper credits +1 at retain direct:retain:retain borrowed value (app.fai:4) aux=none (sample 0x100)"
            ),
            "{report}"
        );
        reset(false, None);
    }

    #[test]
    fn grouped_report_collapses_live_credits_by_site() {
        reset(true, Some(dbg_with_site()));
        record_event(OwnershipOp::Retain.id() as i32, 7, obj(0x100), 0);
        record_event(OwnershipOp::Retain.id() as i32, 7, obj(0x200), 0);

        let report = render_report().expect("report");
        assert!(
            report.contains(
                "2 x live helper credits +1 at retain direct:retain:retain borrowed value (app.fai:4) aux=none"
            ),
            "{report}"
        );
        reset(false, None);
    }

    #[test]
    fn balanced_helper_credit_is_clean() {
        reset(true, None);
        record_event(OwnershipOp::Transfer.id() as i32, 1, obj(0x200), 0);
        record_event(OwnershipOp::Cleanup.id() as i32, 2, obj(0x200), 0);

        let report = render_report().expect("report");
        assert!(report.contains("2 event(s)"), "{report}");
        assert!(
            report.contains("0 object(s) with helper imbalance"),
            "{report}"
        );
        assert!(!has_imbalance());
        reset(false, None);
    }

    #[test]
    fn missing_retain_or_transfer_is_reported_at_cleanup() {
        reset(true, None);
        record_event(OwnershipOp::Cleanup.id() as i32, 9, obj(0x300), 0);

        let report = render_report().expect("report");
        assert!(
            report.contains("1 object(s) with helper imbalance"),
            "{report}"
        );
        assert!(report.contains("unmatched cleanup"), "{report}");
        assert!(report.contains("site=9"), "{report}");
        assert!(has_imbalance());
        reset(false, None);
    }

    #[test]
    fn duplicate_cleanup_of_tracked_value_is_reported() {
        reset(true, None);
        record_event(OwnershipOp::Transfer.id() as i32, 3, obj(0x400), 0);
        record_event(OwnershipOp::Cleanup.id() as i32, 4, obj(0x400), 0);
        record_event(OwnershipOp::Cleanup.id() as i32, 5, obj(0x400), 0);

        let report = render_report().expect("report");
        assert!(
            report.contains("1 object(s) with helper imbalance"),
            "{report}"
        );
        assert!(report.contains("unmatched cleanup"), "{report}");
        assert!(report.contains("site=5"), "{report}");
        assert!(report.contains("history:"), "{report}");
        assert!(
            report.contains("1 x unmatched cleanup at cleanup site=5 aux=none"),
            "{report}"
        );
        reset(false, None);
    }

    #[test]
    fn transfer_seed_reports_uncredited_store() {
        reset_seeded(OwnershipOp::Transfer);
        record_event(OwnershipOp::Store.id() as i32, 9, obj(0x410), 0);

        let report = render_report().expect("report");
        assert!(
            report.contains("1 object(s) with helper imbalance"),
            "{report}"
        );
        assert!(
            report.contains("uncredited owning store; missing retain/transfer before store"),
            "{report}"
        );
        assert!(report.contains("proof "), "{report}");
        assert!(has_imbalance());
        reset(false, None);
    }

    #[test]
    fn transfer_seed_allows_credited_store() {
        reset_seeded(OwnershipOp::Transfer);
        record_event(OwnershipOp::Transfer.id() as i32, 8, obj(0x420), 0);
        record_event(OwnershipOp::Store.id() as i32, 9, obj(0x420), 0);
        record_event(OwnershipOp::Cleanup.id() as i32, 10, obj(0x420), 0);

        let report = render_report().expect("report");
        assert!(
            report.contains("0 object(s) with helper imbalance"),
            "{report}"
        );
        assert!(!has_imbalance());
        reset(false, None);
    }

    #[test]
    fn cleanup_seed_reports_credit_retired_by_free() {
        reset_seeded(OwnershipOp::Cleanup);
        record_alloc(0x430);
        record_event(OwnershipOp::Transfer.id() as i32, 1, obj(0x430), 0);
        record_free(0x430);

        let report = render_report().expect("report");
        assert!(
            report.contains("1 object(s) with helper imbalance"),
            "{report}"
        );
        assert!(
            report.contains("live helper credit retired by free; missing cleanup before free"),
            "{report}"
        );
        assert!(has_imbalance());
        reset(false, None);
    }

    #[test]
    fn lifecycle_events_treat_live_positive_credits_as_still_owned() {
        reset(true, None);
        record_alloc(0x500);
        record_event(OwnershipOp::Transfer.id() as i32, 1, obj(0x500), 0);

        let report = render_report().expect("report");
        assert!(
            report.contains("0 object(s) with helper imbalance"),
            "{report}"
        );
        assert!(!has_imbalance());
        reset(false, None);
    }

    #[test]
    fn lifecycle_events_retire_positive_credits_at_free() {
        reset(true, None);
        record_alloc(0x600);
        record_event(OwnershipOp::Transfer.id() as i32, 1, obj(0x600), 0);
        record_free(0x600);

        let report = render_report().expect("report");
        assert!(
            report.contains("0 object(s) with helper imbalance"),
            "{report}"
        );
        assert!(!has_imbalance());
        reset(false, None);
    }

    #[test]
    fn lifecycle_events_clear_reused_address_history() {
        reset(true, None);
        record_alloc(0x700);
        record_event(OwnershipOp::Transfer.id() as i32, 1, obj(0x700), 0);
        record_event(OwnershipOp::Cleanup.id() as i32, 2, obj(0x700), 0);
        record_free(0x700);
        record_alloc(0x700);

        let report = render_report().expect("report");
        assert!(
            report.contains("0 object(s) with helper imbalance"),
            "{report}"
        );
        assert!(!report.contains("site=1"), "{report}");
        assert!(!has_imbalance());
        reset(false, None);
    }
}
