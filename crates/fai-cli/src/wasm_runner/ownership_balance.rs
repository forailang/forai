use std::cell::RefCell;
use std::collections::HashMap;

use fai_compiler::ownership_abi::OwnershipOp;

use super::nan_box::{ADDR_MASK, QNAN, SIGN_BIT};

#[derive(Debug, Clone)]
struct OwnershipEvent {
    op: OwnershipOp,
    site: u32,
    value: i64,
    addr: Option<u32>,
    aux: i32,
}

#[derive(Debug, Default)]
struct OwnershipLedger {
    enabled: bool,
    events: Vec<OwnershipEvent>,
    credits: HashMap<u32, i32>,
    unmatched: Vec<OwnershipEvent>,
}

thread_local! {
    static LEDGER: RefCell<OwnershipLedger> = RefCell::new(OwnershipLedger::default());
}

pub(crate) fn reset(enabled: bool) {
    LEDGER.with(|ledger| {
        *ledger.borrow_mut() = OwnershipLedger {
            enabled,
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
        }
        ledger.events.push(event);
    });
}

pub(crate) fn render_report() -> Option<String> {
    LEDGER.with(|ledger| {
        let ledger = ledger.borrow();
        if !ledger.enabled {
            return None;
        }
        let imbalanced: Vec<(u32, i32)> = ledger
            .credits
            .iter()
            .filter_map(|(&addr, &credits)| (credits != 0).then_some((addr, credits)))
            .collect();
        let imbalance_count = imbalanced.len() + ledger.unmatched.len();
        let mut out = format!(
            "[ownership-check] {} event(s), {} object(s) with helper imbalance",
            ledger.events.len(),
            imbalance_count
        );
        for (addr, credits) in imbalanced.iter().take(8) {
            out.push_str(&format!("\n  0x{addr:x}: helper credits {credits:+}"));
            if let Some(event) = ledger.events.iter().rev().find(|e| e.addr == Some(*addr)) {
                out.push_str(&format!(
                    " after {} site={} aux={} value=0x{:x}",
                    event.op.name(),
                    event.site,
                    event.aux,
                    event.value as u64
                ));
            }
        }
        for event in ledger
            .unmatched
            .iter()
            .take(8usize.saturating_sub(imbalanced.len()))
        {
            out.push_str(&format!(
                "\n  unmatched {} site={} aux={} value=0x{:x}",
                event.op.name(),
                event.site,
                event.aux,
                event.value as u64
            ));
        }
        Some(out)
    })
}

fn allows_untracked_consume(op: OwnershipOp) -> bool {
    matches!(op, OwnershipOp::Discard | OwnershipOp::Release)
}

fn object_addr(value: i64) -> Option<u32> {
    let bits = value as u64;
    ((bits & (QNAN | SIGN_BIT)) == (QNAN | SIGN_BIT)).then_some((bits & ADDR_MASK) as u32)
}

#[cfg(test)]
mod tests {
    use super::super::nan_box;
    use super::*;

    fn obj(addr: u32) -> i64 {
        (nan_box::QNAN | nan_box::SIGN_BIT | addr as u64) as i64
    }

    #[test]
    fn report_names_imbalanced_helper_credit() {
        reset(true);
        record_event(OwnershipOp::Retain.id() as i32, 7, obj(0x100), 0);

        let report = render_report().expect("report");
        assert!(report.contains("1 event(s)"), "{report}");
        assert!(report.contains("0x100"), "{report}");
        assert!(report.contains("helper credits +1"), "{report}");
        reset(false);
    }

    #[test]
    fn balanced_helper_credit_is_clean() {
        reset(true);
        record_event(OwnershipOp::Transfer.id() as i32, 1, obj(0x200), 0);
        record_event(OwnershipOp::Cleanup.id() as i32, 2, obj(0x200), 0);

        let report = render_report().expect("report");
        assert!(report.contains("2 event(s)"), "{report}");
        assert!(
            report.contains("0 object(s) with helper imbalance"),
            "{report}"
        );
        reset(false);
    }

    #[test]
    fn missing_retain_or_transfer_is_reported_at_cleanup() {
        reset(true);
        record_event(OwnershipOp::Cleanup.id() as i32, 9, obj(0x300), 0);

        let report = render_report().expect("report");
        assert!(
            report.contains("1 object(s) with helper imbalance"),
            "{report}"
        );
        assert!(report.contains("unmatched cleanup"), "{report}");
        assert!(report.contains("site=9"), "{report}");
        reset(false);
    }

    #[test]
    fn duplicate_cleanup_of_tracked_value_is_reported() {
        reset(true);
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
        reset(false);
    }
}
