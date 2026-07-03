use super::*;

/// Number of size-class buckets for the free list. The allocator keeps one
/// free-list head per block size (`block_size / 8`) for blocks up to
/// `NUM_FREE_BUCKETS * 8` bytes, so alloc/free are O(1) instead of an O(n)
/// linear exact-fit scan that degrades as a long-running server accumulates
/// freed blocks (see memory `allocator-freelist-on-degradation`). Larger blocks
/// fall back to a single linear list (rare, stays short). The bucket-head array
/// lives in a reserved guest-memory region at `bucket_base` (zero-initialised).
pub const NUM_FREE_BUCKETS: u32 = 1024;

/// Bytes the bucket-head array occupies (one i32 head per bucket).
pub const FREE_BUCKET_REGION_BYTES: u32 = NUM_FREE_BUCKETS * 4;

// ── $rt_live_objects() -> i32 — read the live-object counter (plan 115) ──
// The counter's global index depends on the module layout, so it's captured
// here at emit time. The `__liveObjects()` debug builtin calls this and boxes
// the result as an Int.
pub(super) fn emit_live_objects(live_count_global: u32) -> Function {
    let mut f = Function::new([]);
    f.instruction(&Instruction::GlobalGet(live_count_global));
    f.instruction(&Instruction::End);
    f
}

/// FAI_HEAP_VERIFY: emit a loop scanning every free-bucket head for an
/// implausible pointer (TRAP_FREELIST_CORRUPT) or an overwritten poison
/// tag (TRAP_FREED_DIRTY). `idx_local`/`node_local` are caller-provided
/// scratch i32 locals. Emitted into rt_alloc/rt_retain/rt_release under
/// the env flag so a stale-pointer write is caught within a statement
/// or two of the writer, with the writer's backtrace.
pub(super) fn emit_heads_scan(
    f: &mut Function,
    bucket_base: u32,
    import_remap: &[Option<u32>],
    idx_local: u32,
    node_local: u32,
) {
    let off8 = MemArg {
        offset: 8,
        align: 0,
        memory_index: 0,
    };
    f.instruction(&Instruction::I32Const(0));
    f.instruction(&Instruction::LocalSet(idx_local));
    f.instruction(&Instruction::Block(wasm_encoder::BlockType::Empty));
    f.instruction(&Instruction::Loop(wasm_encoder::BlockType::Empty));
    f.instruction(&Instruction::LocalGet(idx_local));
    f.instruction(&Instruction::I32Const(NUM_FREE_BUCKETS as i32));
    f.instruction(&Instruction::I32GeU);
    f.instruction(&Instruction::BrIf(1)); // idx >= buckets → done
                                          // node = mem[bucket_base + idx*4]
    f.instruction(&Instruction::I32Const(bucket_base as i32));
    f.instruction(&Instruction::LocalGet(idx_local));
    f.instruction(&Instruction::I32Const(4));
    f.instruction(&Instruction::I32Mul);
    f.instruction(&Instruction::I32Add);
    f.instruction(&Instruction::I32Load(mem0()));
    f.instruction(&Instruction::LocalSet(node_local));
    f.instruction(&Instruction::LocalGet(node_local));
    f.instruction(&Instruction::If(wasm_encoder::BlockType::Empty));
    // corrupt = (node & 7) | node < heap_start | node >= heap_ptr
    f.instruction(&Instruction::LocalGet(node_local));
    f.instruction(&Instruction::I32Const(7));
    f.instruction(&Instruction::I32And);
    f.instruction(&Instruction::LocalGet(node_local));
    f.instruction(&Instruction::I32Const(
        (bucket_base + FREE_BUCKET_REGION_BYTES) as i32,
    ));
    f.instruction(&Instruction::I32LtU);
    f.instruction(&Instruction::I32Or);
    f.instruction(&Instruction::LocalGet(node_local));
    f.instruction(&Instruction::GlobalGet(0));
    f.instruction(&Instruction::I32GeU);
    f.instruction(&Instruction::I32Or);
    f.instruction(&Instruction::If(wasm_encoder::BlockType::Empty));
    emit_trap_report_unreachable(
        f,
        import_remap,
        TRAP_FREELIST_CORRUPT,
        |f| {
            f.instruction(&Instruction::LocalGet(node_local));
            f.instruction(&Instruction::I64ExtendI32U);
        },
        |f| {
            f.instruction(&Instruction::GlobalGet(0));
            f.instruction(&Instruction::I64ExtendI32U);
        },
    );
    f.instruction(&Instruction::End);
    // dirty = mem[node+8] != OBJ_TAG_POISON (frees poison under
    // FAI_RC_CHECK — FAI_HEAP_VERIFY implies users set both).
    // `b` packs (bucket_idx << 32 | tag_word) so the report can name
    // the block's size class alongside the overwriting value.
    f.instruction(&Instruction::LocalGet(node_local));
    f.instruction(&Instruction::I32Load(off8));
    f.instruction(&Instruction::I32Const(OBJ_TAG_POISON));
    f.instruction(&Instruction::I32Ne);
    f.instruction(&Instruction::If(wasm_encoder::BlockType::Empty));
    emit_trap_report_unreachable(
        f,
        import_remap,
        TRAP_FREED_DIRTY,
        |f| {
            f.instruction(&Instruction::LocalGet(node_local));
            f.instruction(&Instruction::I64ExtendI32U);
        },
        |f| {
            f.instruction(&Instruction::LocalGet(idx_local));
            f.instruction(&Instruction::I64ExtendI32U);
            f.instruction(&Instruction::I64Const(32));
            f.instruction(&Instruction::I64Shl);
            f.instruction(&Instruction::LocalGet(node_local));
            f.instruction(&Instruction::I32Load(off8));
            f.instruction(&Instruction::I64ExtendI32U);
            f.instruction(&Instruction::I64Or);
        },
    );
    f.instruction(&Instruction::End);
    f.instruction(&Instruction::End); // node != 0
    f.instruction(&Instruction::LocalGet(idx_local));
    f.instruction(&Instruction::I32Const(1));
    f.instruction(&Instruction::I32Add);
    f.instruction(&Instruction::LocalSet(idx_local));
    f.instruction(&Instruction::Br(0));
    f.instruction(&Instruction::End); // loop
    f.instruction(&Instruction::End); // block
}

// ── $rt_alloc(size: i32) -> i32 ───────────────────────────────────
// Bump allocate `size` bytes (aligned to 8). Returns address.

pub(super) fn emit_alloc(
    freelist_global: u32,
    live_count_global: u32,
    bucket_base: u32,
    import_remap: &[Option<u32>],
) -> Function {
    // locals: 1=addr, 2=new_ptr, 3=mem_bytes, 4=prev/bucket_addr, 5=cur/head,
    // 6=orig_size, 7=bucket_idx, 8/9=verify scan idx/node (all i32)
    let check_leaks = check_leaks_enabled();
    let rc_check = std::env::var_os("FAI_RC_CHECK").is_some();
    let heap_verify = std::env::var_os("FAI_HEAP_VERIFY").is_some();
    let mem_watch = std::env::var_os("FAI_MEM_WATCH").is_some();
    let mut f = Function::new([(9, ValType::I32)]);
    let off4 = MemArg {
        offset: 4,
        align: 0,
        memory_index: 0,
    };
    let off8 = MemArg {
        offset: 8,
        align: 0,
        memory_index: 0,
    };
    // Checked-mode free-list validation (plan 116): a node in local 5 is
    // about to be reused. Trap with a named reason if its address is
    // implausible (link word overwritten → TRAP_FREELIST_CORRUPT) or if
    // its poisoned tag word was overwritten while it sat on the free
    // list (write-after-free → TRAP_FREED_DIRTY). rt_free poisons every
    // freed block's tag under FAI_RC_CHECK, so a clean node always
    // reads OBJ_TAG_POISON here.
    let validate_node = |f: &mut Function, bucket_base: u32| {
        // corrupt = (node & 7) != 0  |  node < heap_start  |  node >= heap_ptr
        f.instruction(&Instruction::LocalGet(5));
        f.instruction(&Instruction::I32Const(7));
        f.instruction(&Instruction::I32And);
        f.instruction(&Instruction::LocalGet(5));
        f.instruction(&Instruction::I32Const(
            (bucket_base + FREE_BUCKET_REGION_BYTES) as i32,
        ));
        f.instruction(&Instruction::I32LtU);
        f.instruction(&Instruction::I32Or);
        f.instruction(&Instruction::LocalGet(5));
        f.instruction(&Instruction::GlobalGet(0));
        f.instruction(&Instruction::I32GeU);
        f.instruction(&Instruction::I32Or);
        f.instruction(&Instruction::If(wasm_encoder::BlockType::Empty));
        emit_trap_report_unreachable(
            f,
            import_remap,
            TRAP_FREELIST_CORRUPT,
            |f| {
                f.instruction(&Instruction::LocalGet(5));
                f.instruction(&Instruction::I64ExtendI32U);
            },
            |f| {
                f.instruction(&Instruction::GlobalGet(0));
                f.instruction(&Instruction::I64ExtendI32U);
            },
        );
        f.instruction(&Instruction::End);
        // dirty = mem[node+8] != OBJ_TAG_POISON
        f.instruction(&Instruction::LocalGet(5));
        f.instruction(&Instruction::I32Load(off8));
        f.instruction(&Instruction::I32Const(OBJ_TAG_POISON));
        f.instruction(&Instruction::I32Ne);
        f.instruction(&Instruction::If(wasm_encoder::BlockType::Empty));
        emit_trap_report_unreachable(
            f,
            import_remap,
            TRAP_FREED_DIRTY,
            |f| {
                f.instruction(&Instruction::LocalGet(5));
                f.instruction(&Instruction::I64ExtendI32U);
            },
            |f| {
                f.instruction(&Instruction::LocalGet(5));
                f.instruction(&Instruction::I32Load(off8));
                f.instruction(&Instruction::I64ExtendI32U);
            },
        );
        f.instruction(&Instruction::End);
    };
    // `--check-leaks` ledger event: __fai_alloc_event(base+8, logical_size)
    // right before each return path hands out the logical pointer.
    let alloc_event = |f: &mut Function, base_local: u32| {
        f.instruction(&Instruction::LocalGet(base_local));
        f.instruction(&Instruction::I32Const(8));
        f.instruction(&Instruction::I32Add);
        f.instruction(&Instruction::LocalGet(6));
        emit_import_call(f, IMPORT_ALLOC_EVENT, import_remap);
    };
    // FAI_HEAP_VERIFY (plan 116): scan every free-bucket HEAD on every
    // allocation and trap at the first implausible or dirtied node. This
    // narrows "something wrote through a stale pointer while the block
    // sat on the free list" from detection-at-reuse (whenever that bucket
    // is next popped — possibly thousands of allocs later) down to the
    // first allocation after the bad write, so the trap backtrace lands
    // next to the writer. Heads-only keeps it O(NUM_FREE_BUCKETS) per
    // alloc; a mid-chain dirty node surfaces once it becomes head.
    if heap_verify {
        emit_heads_scan(&mut f, bucket_base, import_remap, 8, 9);
    }
    if mem_watch {
        emit_import_call(&mut f, IMPORT_MEM_WATCH, import_remap);
    }
    // Stash the LOGICAL size requested (before the rc-prefix inflation below) so
    // each return path can stamp it into the prefix's spare word at obj_addr-4.
    // RT_RELEASE reads it back to free the block at its true allocated size —
    // load-bearing for objects whose logical size differs from a count-derived
    // formula (dicts over-allocate spare capacity for in-place `set` growth;
    // plan 115). The slot is the same word `rt_free` later reuses as the
    // free-list `next` link, so every alloc must re-stamp it.
    f.instruction(&Instruction::LocalGet(0));
    f.instruction(&Instruction::LocalSet(6));
    // Live-object counter (plan 113 oracle): every alloc produces exactly one
    // object (or traps on grow failure), so bump it once up front.
    f.instruction(&Instruction::GlobalGet(live_count_global));
    f.instruction(&Instruction::I32Const(1));
    f.instruction(&Instruction::I32Add);
    f.instruction(&Instruction::GlobalSet(live_count_global));
    // ── refcount prefix (plan 113) ──
    // Reserve 8 extra bytes in front of the object for its reference count.
    // The block base holds `rc`; the logical object pointer we hand back is
    // base+8, so `tag@0`, `count@4` and all payload offsets are unchanged. We
    // inflate the request here so the free-list search and bump below operate on
    // the real block size; each return path writes rc=1 and yields base+8.
    f.instruction(&Instruction::LocalGet(0));
    f.instruction(&Instruction::I32Const(8));
    f.instruction(&Instruction::I32Add);
    // Round the block size UP to a multiple of 8. The bump path aligns the next
    // pointer to 8 anyway, so this doesn't change footprint — but it makes every
    // block size an exact multiple of 8, which the size-bucketing below relies on:
    // `block_size / 8` must round-trip exactly, or a bucket would mix sizes in
    // `[idx*8, idx*8+7]` and a larger request could reuse a too-small freed block.
    f.instruction(&Instruction::I32Const(7));
    f.instruction(&Instruction::I32Add);
    f.instruction(&Instruction::I32Const(!7));
    f.instruction(&Instruction::I32And);
    f.instruction(&Instruction::LocalSet(0));
    // FAI_ALLOC_GUARD: trap on any single allocation past 256 MB. No
    // forai value legitimately needs that in one block, so a request
    // this large is a runaway (a concat loop building an ever-bigger
    // string, an array/dict blowup) — trapping here names the size and
    // the backtrace instead of letting the bump path grow memory toward
    // the 4 GB ceiling and thrash. Diagnostic-only (off unless the env
    // is set at codegen or `--checked` is on) so production allocs pay
    // nothing.
    if std::env::var_os("FAI_ALLOC_GUARD").is_some() || checked_enabled() {
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&Instruction::I32Const(0x1000_0000)); // 256 MiB
        f.instruction(&Instruction::I32GeU);
        f.instruction(&Instruction::If(wasm_encoder::BlockType::Empty));
        emit_trap_report_unreachable(
            &mut f,
            import_remap,
            TRAP_ALLOC_TOO_BIG,
            |f| {
                f.instruction(&Instruction::LocalGet(6)); // requested logical size
                f.instruction(&Instruction::I64ExtendI32U);
            },
            |f| {
                f.instruction(&Instruction::LocalGet(0)); // rounded block size
                f.instruction(&Instruction::I64ExtendI32U);
            },
        );
        f.instruction(&Instruction::End);
    }
    // ── reuse from the free list first ──
    // Free lists are SIZE-BUCKETED: one head per block size (`block_size/8`) in
    // the reserved region at `bucket_base`, so reuse is O(1) — no linear scan
    // that degrades as a long-running server accumulates freed blocks (memory
    // `allocator-freelist-on-degradation`). Blocks larger than the bucketed range
    // fall back to a single linear list (`freelist_global`); those are rare so it
    // stays short. A freed block stores [size@0, next@4].
    // bucket_idx = block_size / 8
    f.instruction(&Instruction::LocalGet(0));
    f.instruction(&Instruction::I32Const(3));
    f.instruction(&Instruction::I32ShrU);
    f.instruction(&Instruction::LocalSet(7));
    f.instruction(&Instruction::LocalGet(7));
    f.instruction(&Instruction::I32Const(NUM_FREE_BUCKETS as i32));
    f.instruction(&Instruction::I32LtU);
    f.instruction(&Instruction::If(wasm_encoder::BlockType::Empty));
    // ── small: O(1) bucket pop ──
    // bucket_addr = bucket_base + idx*4  (local 4)
    f.instruction(&Instruction::I32Const(bucket_base as i32));
    f.instruction(&Instruction::LocalGet(7));
    f.instruction(&Instruction::I32Const(4));
    f.instruction(&Instruction::I32Mul);
    f.instruction(&Instruction::I32Add);
    f.instruction(&Instruction::LocalSet(4));
    // head = mem[bucket_addr]  (local 5)
    f.instruction(&Instruction::LocalGet(4));
    f.instruction(&Instruction::I32Load(mem0()));
    f.instruction(&Instruction::LocalSet(5));
    // if head != 0: pop + return
    f.instruction(&Instruction::LocalGet(5));
    f.instruction(&Instruction::If(wasm_encoder::BlockType::Empty));
    if rc_check {
        validate_node(&mut f, bucket_base);
    }
    // mem[bucket_addr] = head.next
    f.instruction(&Instruction::LocalGet(4));
    f.instruction(&Instruction::LocalGet(5));
    f.instruction(&Instruction::I32Load(off4));
    f.instruction(&Instruction::I32Store(mem0()));
    // rc=1 at base, logical size at base+4, return base+8
    f.instruction(&Instruction::LocalGet(5));
    f.instruction(&Instruction::I32Const(1));
    f.instruction(&Instruction::I32Store(mem0()));
    f.instruction(&Instruction::LocalGet(5));
    f.instruction(&Instruction::LocalGet(6));
    f.instruction(&Instruction::I32Store(off4));
    if check_leaks {
        alloc_event(&mut f, 5);
    }
    f.instruction(&Instruction::LocalGet(5));
    f.instruction(&Instruction::I32Const(8));
    f.instruction(&Instruction::I32Add);
    f.instruction(&Instruction::Return);
    f.instruction(&Instruction::End); // head != 0
                                      // else: empty bucket → fall through to bump.
    f.instruction(&Instruction::Else);
    // ── large: linear exact-fit scan of the fallback list ──
    f.instruction(&Instruction::I32Const(0));
    f.instruction(&Instruction::LocalSet(4)); // prev = 0 (null: cur is head)
    f.instruction(&Instruction::GlobalGet(freelist_global));
    f.instruction(&Instruction::LocalSet(5)); // cur = head
    f.instruction(&Instruction::Block(wasm_encoder::BlockType::Empty));
    f.instruction(&Instruction::Loop(wasm_encoder::BlockType::Empty));
    f.instruction(&Instruction::LocalGet(5));
    f.instruction(&Instruction::I32Eqz);
    f.instruction(&Instruction::BrIf(1)); // cur == 0 → not found, break to bump
    if rc_check {
        validate_node(&mut f, bucket_base);
    }
    f.instruction(&Instruction::LocalGet(5));
    f.instruction(&Instruction::I32Load(mem0()));
    f.instruction(&Instruction::LocalGet(0));
    f.instruction(&Instruction::I32Eq);
    f.instruction(&Instruction::If(wasm_encoder::BlockType::Empty));
    f.instruction(&Instruction::LocalGet(4));
    f.instruction(&Instruction::I32Eqz);
    f.instruction(&Instruction::If(wasm_encoder::BlockType::Empty));
    f.instruction(&Instruction::LocalGet(5));
    f.instruction(&Instruction::I32Load(off4));
    f.instruction(&Instruction::GlobalSet(freelist_global));
    f.instruction(&Instruction::Else);
    f.instruction(&Instruction::LocalGet(4));
    f.instruction(&Instruction::LocalGet(5));
    f.instruction(&Instruction::I32Load(off4));
    f.instruction(&Instruction::I32Store(off4));
    f.instruction(&Instruction::End);
    f.instruction(&Instruction::LocalGet(5));
    f.instruction(&Instruction::I32Const(1));
    f.instruction(&Instruction::I32Store(mem0()));
    f.instruction(&Instruction::LocalGet(5));
    f.instruction(&Instruction::LocalGet(6));
    f.instruction(&Instruction::I32Store(off4));
    if check_leaks {
        alloc_event(&mut f, 5);
    }
    f.instruction(&Instruction::LocalGet(5));
    f.instruction(&Instruction::I32Const(8));
    f.instruction(&Instruction::I32Add);
    f.instruction(&Instruction::Return);
    f.instruction(&Instruction::End);
    f.instruction(&Instruction::LocalGet(5));
    f.instruction(&Instruction::LocalSet(4));
    f.instruction(&Instruction::LocalGet(5));
    f.instruction(&Instruction::I32Load(off4));
    f.instruction(&Instruction::LocalSet(5));
    f.instruction(&Instruction::Br(0)); // continue
    f.instruction(&Instruction::End); // loop
    f.instruction(&Instruction::End); // block
    f.instruction(&Instruction::End); // idx < NUM_FREE_BUCKETS
                                      // addr = heap_ptr (global 0)
    f.instruction(&Instruction::GlobalGet(0));
    f.instruction(&Instruction::LocalSet(1));
    // new_ptr = (heap_ptr + size + 7) & ~7  (align to 8)
    f.instruction(&Instruction::GlobalGet(0));
    f.instruction(&Instruction::LocalGet(0));
    f.instruction(&Instruction::I32Const(7));
    f.instruction(&Instruction::I32Add);
    f.instruction(&Instruction::I32Const(!7)); // ~7 = 0xFFFFFFF8
    f.instruction(&Instruction::I32And);
    f.instruction(&Instruction::I32Add);
    f.instruction(&Instruction::LocalSet(2));
    // mem_bytes = memory.size * 65536
    f.instruction(&Instruction::MemorySize(0));
    f.instruction(&Instruction::I32Const(65536));
    f.instruction(&Instruction::I32Mul);
    f.instruction(&Instruction::LocalSet(3));
    // Grow memory while new_ptr would exceed current mem_bytes.
    // memory.grow takes pages as argument, returns prev-page-count or
    // -1 on failure. We grow in 16-page chunks (1 MiB) until new_ptr
    // fits, trapping if grow returns -1.
    //
    // Without this, the Rust wasmtime host (tests, `forai run`)
    // silently resizes memory on mem.data_mut() access — tests pass.
    // The browser's JS host does not: jsToWasm writes past the end
    // hit a detached buffer, later reads return garbage, and users
    // see heap addresses rendered as ints instead of the expected
    // values. See plan 99 Phase 1 notes.
    f.instruction(&Instruction::Block(wasm_encoder::BlockType::Empty));
    f.instruction(&Instruction::Loop(wasm_encoder::BlockType::Empty));
    {
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::LocalGet(3));
        f.instruction(&Instruction::I32LeU);
        f.instruction(&Instruction::BrIf(1)); // break: new_ptr fits
                                              // memory.grow(16)
        f.instruction(&Instruction::I32Const(16));
        f.instruction(&Instruction::MemoryGrow(0));
        // If -1, trap.
        f.instruction(&Instruction::I32Const(-1));
        f.instruction(&Instruction::I32Eq);
        f.instruction(&Instruction::If(wasm_encoder::BlockType::Empty));
        emit_trap_report_unreachable(
            &mut f,
            import_remap,
            TRAP_OOM,
            |f| {
                f.instruction(&Instruction::LocalGet(6)); // requested size
                f.instruction(&Instruction::I64ExtendI32U);
            },
            |f| {
                f.instruction(&Instruction::LocalGet(2)); // needed heap ptr
                f.instruction(&Instruction::I64ExtendI32U);
            },
        );
        f.instruction(&Instruction::End);
        // mem_bytes += 16 * 65536 = 1 MiB
        f.instruction(&Instruction::LocalGet(3));
        f.instruction(&Instruction::I32Const(16 * 65536));
        f.instruction(&Instruction::I32Add);
        f.instruction(&Instruction::LocalSet(3));
        f.instruction(&Instruction::Br(0)); // continue
    }
    f.instruction(&Instruction::End); // end loop
    f.instruction(&Instruction::End); // end block
                                      // heap_ptr = new_ptr
    f.instruction(&Instruction::LocalGet(2));
    f.instruction(&Instruction::GlobalSet(0));
    // bumped block: rc=1 at base, logical size at base+4, return base+8
    f.instruction(&Instruction::LocalGet(1));
    f.instruction(&Instruction::I32Const(1));
    f.instruction(&Instruction::I32Store(mem0()));
    f.instruction(&Instruction::LocalGet(1));
    f.instruction(&Instruction::LocalGet(6));
    f.instruction(&Instruction::I32Store(off4));
    if check_leaks {
        alloc_event(&mut f, 1);
    }
    f.instruction(&Instruction::LocalGet(1));
    f.instruction(&Instruction::I32Const(8));
    f.instruction(&Instruction::I32Add);
    f.instruction(&Instruction::End);
    f
}

// ── $rt_free(ptr: i32, size: i32) ────────────────────────────────
// Return a heap block to the free list: store [size@0, next@4] in the
// block and make it the new list head. `size` is the block's original
// alloc size so a later same-size `alloc` reuses it. Blocks are always
// >= 8 bytes (every heap object is), so the [size,next] header fits.
pub(super) fn emit_free(
    freelist_global: u32,
    live_count_global: u32,
    bucket_base: u32,
    import_remap: &[Option<u32>],
) -> Function {
    // params: 0 = ptr (i32, logical obj ptr), 1 = size (i32, logical obj size)
    // locals: 2 = bucket_idx, 3 = bucket_addr (i32)
    let rc_check = std::env::var_os("FAI_RC_CHECK").is_some();
    // FAI_NO_REUSE (UAF hunt): orphan every freed block instead of putting
    // it back on the free list. Combined with FAI_RC_CHECK's poison, a
    // freed block then stays poisoned forever (never reused/un-poisoned),
    // so a stale retain/release/access of a dangling reference traps AT
    // THE ACT (TRAP_RC_*_POISON) with the offending backtrace — catching
    // the corruptor, not just the downstream dirtied block. Leaks heavily;
    // diagnostic-only.
    let no_reuse = std::env::var_os("FAI_NO_REUSE").is_some();
    let mut f = Function::new([(2, ValType::I32)]);
    // `--check-leaks` ledger event, with the logical ptr/size before the
    // rc-prefix conversion below rewrites them to block base/size.
    if check_leaks_enabled() {
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&Instruction::LocalGet(1));
        emit_import_call(&mut f, IMPORT_FREE_EVENT, import_remap);
    }
    // Live-object counter (plan 113 oracle): one object reclaimed per free.
    f.instruction(&Instruction::GlobalGet(live_count_global));
    f.instruction(&Instruction::I32Const(1));
    f.instruction(&Instruction::I32Sub);
    f.instruction(&Instruction::GlobalSet(live_count_global));
    // Refcount prefix (plan 113): the real block starts 8 bytes before the
    // logical pointer and is 8 bytes larger. Convert to the real base/size so
    // the free-list node covers the whole block (including the rc prefix) and a
    // later same-size alloc reuses it exactly.
    f.instruction(&Instruction::LocalGet(0));
    f.instruction(&Instruction::I32Const(8));
    f.instruction(&Instruction::I32Sub);
    f.instruction(&Instruction::LocalSet(0));
    f.instruction(&Instruction::LocalGet(1));
    f.instruction(&Instruction::I32Const(8));
    f.instruction(&Instruction::I32Add);
    // Round UP to a multiple of 8 to match emit_alloc's block size exactly, so a
    // freed block lands in the same bucket a same-size alloc will look in.
    f.instruction(&Instruction::I32Const(7));
    f.instruction(&Instruction::I32Add);
    f.instruction(&Instruction::I32Const(!7));
    f.instruction(&Instruction::I32And);
    f.instruction(&Instruction::LocalSet(1));
    // Checked-mode (plan 116): catch bad frees AT the free site, before
    // they poison the free list. A misaligned/out-of-heap base is a
    // garbage pointer (TRAP_FREELIST_CORRUPT); a base whose tag word is
    // already OBJ_TAG_POISON was freed before (TRAP_DOUBLE_FREE).
    if rc_check {
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&Instruction::I32Const(7));
        f.instruction(&Instruction::I32And);
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&Instruction::I32Const(
            (bucket_base + FREE_BUCKET_REGION_BYTES) as i32,
        ));
        f.instruction(&Instruction::I32LtU);
        f.instruction(&Instruction::I32Or);
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&Instruction::GlobalGet(0));
        f.instruction(&Instruction::I32GeU);
        f.instruction(&Instruction::I32Or);
        f.instruction(&Instruction::If(wasm_encoder::BlockType::Empty));
        emit_trap_report_unreachable(
            &mut f,
            import_remap,
            TRAP_FREELIST_CORRUPT,
            |f| {
                f.instruction(&Instruction::LocalGet(0));
                f.instruction(&Instruction::I64ExtendI32U);
            },
            |f| {
                f.instruction(&Instruction::GlobalGet(0));
                f.instruction(&Instruction::I64ExtendI32U);
            },
        );
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&Instruction::I32Load(MemArg {
            offset: 8,
            align: 0,
            memory_index: 0,
        }));
        f.instruction(&Instruction::I32Const(OBJ_TAG_POISON));
        f.instruction(&Instruction::I32Eq);
        f.instruction(&Instruction::If(wasm_encoder::BlockType::Empty));
        emit_trap_report_unreachable(
            &mut f,
            import_remap,
            TRAP_DOUBLE_FREE,
            |f| {
                f.instruction(&Instruction::LocalGet(0));
                f.instruction(&Instruction::I64ExtendI32U);
            },
            |f| {
                f.instruction(&Instruction::LocalGet(1));
                f.instruction(&Instruction::I64ExtendI32U);
            },
        );
        f.instruction(&Instruction::End);
    }
    // Push onto the SIZE-BUCKETED free list (O(1)); blocks too large for the
    // bucketed range go on the single linear fallback list. Mirrors emit_alloc.
    // Skipped entirely under FAI_NO_REUSE (the block is orphaned).
    if !no_reuse {
        // bucket_idx = block_size / 8
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&Instruction::I32Const(3));
        f.instruction(&Instruction::I32ShrU);
        f.instruction(&Instruction::LocalSet(2));
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::I32Const(NUM_FREE_BUCKETS as i32));
        f.instruction(&Instruction::I32LtU);
        f.instruction(&Instruction::If(wasm_encoder::BlockType::Empty));
        // ── small: push to bucket[idx] ──
        // bucket_addr = bucket_base + idx*4  (local 3)
        f.instruction(&Instruction::I32Const(bucket_base as i32));
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::I32Const(4));
        f.instruction(&Instruction::I32Mul);
        f.instruction(&Instruction::I32Add);
        f.instruction(&Instruction::LocalSet(3));
        // block.next (base+4) = mem[bucket_addr]
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&Instruction::LocalGet(3));
        f.instruction(&Instruction::I32Load(mem0()));
        f.instruction(&Instruction::I32Store(MemArg {
            offset: 4,
            align: 0,
            memory_index: 0,
        }));
        // mem[bucket_addr] = base
        f.instruction(&Instruction::LocalGet(3));
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&Instruction::I32Store(mem0()));
        f.instruction(&Instruction::Else);
        // ── large: [size@0, next@4] on the linear fallback list ──
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&Instruction::I32Store(mem0()));
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&Instruction::GlobalGet(freelist_global));
        f.instruction(&Instruction::I32Store(MemArg {
            offset: 4,
            align: 0,
            memory_index: 0,
        }));
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&Instruction::GlobalSet(freelist_global));
        f.instruction(&Instruction::End);
    } // !no_reuse
      // Checked-mode: poison the object tag slot (base+8 = the logical obj_addr,
      // untouched by the free-list node at base/base+4) so a stale reference that
      // reaches an RC op before the block is reused traps. (plan 113 R2)
    if rc_check {
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&Instruction::I32Const(OBJ_TAG_POISON));
        f.instruction(&Instruction::I32Store(MemArg {
            offset: 8,
            align: 0,
            memory_index: 0,
        }));
    }
    f.instruction(&Instruction::End);
    f
}

// ── $rt_retain(v: i64) -> i64 — reference-count increment (plan 113) ──
// Bump the count in the 8-byte prefix at obj_addr-8; no-op + passthrough for
// primitives. Returns `v` so call sites can retain inline.
pub(super) fn emit_retain(base: u32, bucket_base: u32, import_remap: &[Option<u32>]) -> Function {
    // param 0: v (i64). local 1 = rc slot address; 2/3 = scan scratch (i32).
    let rc_check = std::env::var_os("FAI_RC_CHECK").is_some();
    let heap_verify = std::env::var_os("FAI_HEAP_VERIFY").is_some();
    let mem_watch = std::env::var_os("FAI_MEM_WATCH").is_some();
    let rc_watch = std::env::var_os("FAI_RC_WATCH").is_some();
    let mut f = Function::new([(3, ValType::I32)]);
    let empty = wasm_encoder::BlockType::Empty;
    if heap_verify {
        emit_heads_scan(&mut f, bucket_base, import_remap, 2, 3);
    }
    if mem_watch {
        emit_import_call(&mut f, IMPORT_MEM_WATCH, import_remap);
    }
    f.instruction(&Instruction::LocalGet(0));
    f.instruction(&Instruction::Call(base + RT_IS_OBJ));
    f.instruction(&Instruction::If(empty));
    // rc_slot = obj_addr(v) - 8
    f.instruction(&Instruction::LocalGet(0));
    f.instruction(&Instruction::Call(base + RT_OBJ_ADDR));
    f.instruction(&Instruction::I32Const(8));
    f.instruction(&Instruction::I32Sub);
    f.instruction(&Instruction::LocalSet(1));
    // RC watchpoint: __fai_rc_watch(obj_addr=rc_slot+8, rc_slot, +1).
    if rc_watch {
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&Instruction::I32Const(8));
        f.instruction(&Instruction::I32Add);
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&Instruction::I32Const(1));
        emit_import_call(&mut f, IMPORT_RC_WATCH, import_remap);
    }
    // Checked-mode: trap on retaining a freed object (tag at rc_slot+8 poisoned).
    if rc_check {
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&Instruction::I32Load(MemArg {
            offset: 8,
            align: 0,
            memory_index: 0,
        }));
        f.instruction(&Instruction::I32Const(OBJ_TAG_POISON));
        f.instruction(&Instruction::I32Eq);
        f.instruction(&Instruction::If(empty));
        emit_trap_report_unreachable(
            &mut f,
            import_remap,
            TRAP_RC_RETAIN_POISON,
            |f| {
                f.instruction(&Instruction::LocalGet(0)); // boxed value
            },
            |f| {
                f.instruction(&Instruction::LocalGet(1)); // rc-slot addr
                f.instruction(&Instruction::I64ExtendI32U);
            },
        );
        f.instruction(&Instruction::End);
    }
    // mem[rc_slot] = mem[rc_slot] + 1
    f.instruction(&Instruction::LocalGet(1));
    f.instruction(&Instruction::LocalGet(1));
    f.instruction(&Instruction::I32Load(mem0()));
    f.instruction(&Instruction::I32Const(1));
    f.instruction(&Instruction::I32Add);
    f.instruction(&Instruction::I32Store(mem0()));
    f.instruction(&Instruction::End);
    // return v
    f.instruction(&Instruction::LocalGet(0));
    f.instruction(&Instruction::End);
    f
}

// ── $rt_release(v: i64) -> () — reference-count decrement; free at zero ──
// Decrement the count at obj_addr-8. At zero, release each child (so their
// counts drop too) and free the block via the per-tag child traversal. No-op on
// primitives (the `is_obj` guard). The acyclic owned graph guarantees the
// recursion terminates.
pub(super) fn emit_release(base: u32, bucket_base: u32, import_remap: &[Option<u32>]) -> Function {
    // param 0: v. locals: 1=addr, 2=tag, 3=count, 4=i, 5=size, 6=entry, 7=rc,
    // 8/9 = FAI_HEAP_VERIFY scan scratch.
    let rc_check = std::env::var_os("FAI_RC_CHECK").is_some();
    let heap_verify = std::env::var_os("FAI_HEAP_VERIFY").is_some();
    let mem_watch = std::env::var_os("FAI_MEM_WATCH").is_some();
    let rc_watch = std::env::var_os("FAI_RC_WATCH").is_some();
    let mut f = Function::new([(9, ValType::I32)]);
    let off4 = MemArg {
        offset: 4,
        align: 0,
        memory_index: 0,
    };
    let empty = wasm_encoder::BlockType::Empty;
    if heap_verify {
        emit_heads_scan(&mut f, bucket_base, import_remap, 8, 9);
    }
    if mem_watch {
        emit_import_call(&mut f, IMPORT_MEM_WATCH, import_remap);
    }

    // if !is_obj(v) { return }
    f.instruction(&Instruction::LocalGet(0));
    f.instruction(&Instruction::Call(base + RT_IS_OBJ));
    f.instruction(&Instruction::I32Eqz);
    f.instruction(&Instruction::If(empty));
    f.instruction(&Instruction::Return);
    f.instruction(&Instruction::End);
    // addr = obj_addr(v)
    f.instruction(&Instruction::LocalGet(0));
    f.instruction(&Instruction::Call(base + RT_OBJ_ADDR));
    f.instruction(&Instruction::LocalSet(1));
    // RC watchpoint: __fai_rc_watch(obj_addr, rc_slot=addr-8, -1).
    if rc_watch {
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&Instruction::I32Const(8));
        f.instruction(&Instruction::I32Sub);
        f.instruction(&Instruction::I32Const(-1));
        emit_import_call(&mut f, IMPORT_RC_WATCH, import_remap);
    }
    // Checked-mode: trap on releasing a freed object (tag poisoned). Catches a
    // stale reference being released a second time. (plan 113 R2)
    if rc_check {
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&Instruction::I32Load(mem0()));
        f.instruction(&Instruction::I32Const(OBJ_TAG_POISON));
        f.instruction(&Instruction::I32Eq);
        f.instruction(&Instruction::If(empty));
        emit_trap_report_unreachable(
            &mut f,
            import_remap,
            TRAP_RC_RELEASE_POISON,
            |f| {
                f.instruction(&Instruction::LocalGet(0)); // boxed value
            },
            |f| {
                f.instruction(&Instruction::LocalGet(1)); // obj addr
                f.instruction(&Instruction::I64ExtendI32U);
            },
        );
        f.instruction(&Instruction::End);
    }
    // rc = mem[addr-8] - 1 ; mem[addr-8] = rc ; if rc != 0 { return }
    f.instruction(&Instruction::LocalGet(1));
    f.instruction(&Instruction::I32Const(8));
    f.instruction(&Instruction::I32Sub); // store address (rc slot)
    f.instruction(&Instruction::LocalGet(1));
    f.instruction(&Instruction::I32Const(8));
    f.instruction(&Instruction::I32Sub);
    f.instruction(&Instruction::I32Load(mem0()));
    f.instruction(&Instruction::I32Const(1));
    f.instruction(&Instruction::I32Sub);
    f.instruction(&Instruction::LocalTee(7)); // rc = old - 1 (kept on stack for store)
    f.instruction(&Instruction::I32Store(mem0()));
    // Checked-mode: a negative count means this object was released more times
    // than retained (double-free / over-release) — the canonical symptom of a
    // mis-classified transfer. Trap. (plan 113 R2)
    if rc_check {
        f.instruction(&Instruction::LocalGet(7));
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::I32LtS);
        f.instruction(&Instruction::If(empty));
        emit_trap_report_unreachable(
            &mut f,
            import_remap,
            TRAP_RC_OVER_RELEASE,
            |f| {
                f.instruction(&Instruction::LocalGet(0)); // boxed value
            },
            |f| {
                f.instruction(&Instruction::LocalGet(7)); // new (negative) rc
                f.instruction(&Instruction::I64ExtendI32S);
            },
        );
        f.instruction(&Instruction::End);
    }
    f.instruction(&Instruction::LocalGet(7));
    f.instruction(&Instruction::I32Const(0));
    f.instruction(&Instruction::I32Ne);
    f.instruction(&Instruction::If(empty));
    f.instruction(&Instruction::Return);
    f.instruction(&Instruction::End);
    // rc hit zero → free children, then self. tag = mem[addr]; count = mem[addr+4]
    f.instruction(&Instruction::LocalGet(1));
    f.instruction(&Instruction::I32Load(mem0()));
    f.instruction(&Instruction::LocalSet(2));
    f.instruction(&Instruction::LocalGet(1));
    f.instruction(&Instruction::I32Load(off4));
    f.instruction(&Instruction::LocalSet(3));
    f.instruction(&Instruction::I32Const(0));
    f.instruction(&Instruction::LocalSet(5));

    let emit_entry_loop =
        |f: &mut Function, entry_base: i32, stride: i32, child_offsets: &[u64]| {
            f.instruction(&Instruction::I32Const(0));
            f.instruction(&Instruction::LocalSet(4)); // i = 0
            f.instruction(&Instruction::Block(empty));
            f.instruction(&Instruction::Loop(empty));
            f.instruction(&Instruction::LocalGet(4));
            f.instruction(&Instruction::LocalGet(3));
            f.instruction(&Instruction::I32GeU);
            f.instruction(&Instruction::BrIf(1)); // i >= count → break
            f.instruction(&Instruction::LocalGet(1));
            f.instruction(&Instruction::I32Const(entry_base));
            f.instruction(&Instruction::I32Add);
            f.instruction(&Instruction::LocalGet(4));
            f.instruction(&Instruction::I32Const(stride));
            f.instruction(&Instruction::I32Mul);
            f.instruction(&Instruction::I32Add);
            f.instruction(&Instruction::LocalSet(6));
            for &co in child_offsets {
                f.instruction(&Instruction::LocalGet(6));
                f.instruction(&Instruction::I64Load(MemArg {
                    offset: co,
                    align: 0,
                    memory_index: 0,
                }));
                f.instruction(&Instruction::Call(base + RT_RELEASE));
            }
            f.instruction(&Instruction::LocalGet(4));
            f.instruction(&Instruction::I32Const(1));
            f.instruction(&Instruction::I32Add);
            f.instruction(&Instruction::LocalSet(4));
            f.instruction(&Instruction::Br(0)); // continue
            f.instruction(&Instruction::End); // loop
            f.instruction(&Instruction::End); // block
        };

    // STRING → no children; size = 8 + count(len)
    f.instruction(&Instruction::LocalGet(2));
    f.instruction(&Instruction::I32Const(OBJ_TAG_STRING));
    f.instruction(&Instruction::I32Eq);
    f.instruction(&Instruction::If(empty));
    f.instruction(&Instruction::I32Const(8));
    f.instruction(&Instruction::LocalGet(3));
    f.instruction(&Instruction::I32Add);
    f.instruction(&Instruction::LocalSet(5));
    f.instruction(&Instruction::End);
    // ARRAY or TUPLE → child @ +8; size = 8 + count*8
    f.instruction(&Instruction::LocalGet(2));
    f.instruction(&Instruction::I32Const(OBJ_TAG_ARRAY));
    f.instruction(&Instruction::I32Eq);
    f.instruction(&Instruction::LocalGet(2));
    f.instruction(&Instruction::I32Const(OBJ_TAG_TUPLE));
    f.instruction(&Instruction::I32Eq);
    f.instruction(&Instruction::I32Or);
    f.instruction(&Instruction::If(empty));
    emit_entry_loop(&mut f, 8, 8, &[0]);
    f.instruction(&Instruction::I32Const(8));
    f.instruction(&Instruction::LocalGet(3));
    f.instruction(&Instruction::I32Const(8));
    f.instruction(&Instruction::I32Mul);
    f.instruction(&Instruction::I32Add);
    f.instruction(&Instruction::LocalSet(5));
    f.instruction(&Instruction::End);
    // DICT → release (key,val) @ +8, stride 16 for each of `count` live entries.
    // The block SIZE is NOT count-derived: a dict over-allocates spare capacity
    // (`cap = max(16, count+8)`) for in-place `set` growth, and `count` can grow
    // after alloc — so free by the LOGICAL alloc size stamped in the prefix word
    // at obj_addr-4 (plan 115). Using a count formula here under-frees the block,
    // stranding the spare-capacity tail and defeating free-list reuse.
    f.instruction(&Instruction::LocalGet(2));
    f.instruction(&Instruction::I32Const(OBJ_TAG_DICT));
    f.instruction(&Instruction::I32Eq);
    f.instruction(&Instruction::If(empty));
    emit_entry_loop(&mut f, 8, 16, &[0, 8]);
    // size = mem[addr - 4] (the logical alloc size stamped by rt_alloc)
    f.instruction(&Instruction::LocalGet(1));
    f.instruction(&Instruction::I32Const(4));
    f.instruction(&Instruction::I32Sub);
    f.instruction(&Instruction::I32Load(MemArg {
        offset: 0,
        align: 0,
        memory_index: 0,
    }));
    f.instruction(&Instruction::LocalSet(5));
    f.instruction(&Instruction::End);
    // INSTANCE → (key,val) @ +16, stride 16; size = 16 + count*16
    f.instruction(&Instruction::LocalGet(2));
    f.instruction(&Instruction::I32Const(OBJ_TAG_INSTANCE));
    f.instruction(&Instruction::I32Eq);
    f.instruction(&Instruction::If(empty));
    emit_entry_loop(&mut f, 16, 16, &[0, 8]);
    f.instruction(&Instruction::I32Const(16));
    f.instruction(&Instruction::LocalGet(3));
    f.instruction(&Instruction::I32Const(16));
    f.instruction(&Instruction::I32Mul);
    f.instruction(&Instruction::I32Add);
    f.instruction(&Instruction::LocalSet(5));
    f.instruction(&Instruction::End);
    // CLOSURE → upvalues @ +16, stride 8; size = 16 + uv_count*8 (plan 113 R2).
    // `uv_count` lives at addr+8 (addr+4 is the table index), so reload local 3
    // before reusing the entry loop. Releasing each upvalue balances the
    // capture-time retain: a captured-object upvalue drops its ref, and a
    // captured CELL (a NaN-boxed OBJ_TAG_CELL since plan 114) drops the
    // closure's co-ownership of the shared slot — the cell frees when its
    // last owner (enclosing frame or sibling closure) lets go.
    f.instruction(&Instruction::LocalGet(2));
    f.instruction(&Instruction::I32Const(OBJ_TAG_CLOSURE));
    f.instruction(&Instruction::I32Eq);
    f.instruction(&Instruction::If(empty));
    f.instruction(&Instruction::LocalGet(1));
    f.instruction(&Instruction::I32Load(MemArg {
        offset: 8,
        align: 0,
        memory_index: 0,
    }));
    f.instruction(&Instruction::LocalSet(3));
    emit_entry_loop(&mut f, 16, 8, &[0]);
    f.instruction(&Instruction::I32Const(16));
    f.instruction(&Instruction::LocalGet(3));
    f.instruction(&Instruction::I32Const(8));
    f.instruction(&Instruction::I32Mul);
    f.instruction(&Instruction::I32Add);
    f.instruction(&Instruction::LocalSet(5));
    f.instruction(&Instruction::End);
    // CELL → shared mutable slot for a captured-mutated `var` (plan 114):
    // [tag@0][pad@4][value@8], fixed 16 bytes. Release the owned value,
    // then free the block.
    f.instruction(&Instruction::LocalGet(2));
    f.instruction(&Instruction::I32Const(OBJ_TAG_CELL));
    f.instruction(&Instruction::I32Eq);
    f.instruction(&Instruction::If(empty));
    f.instruction(&Instruction::LocalGet(1));
    f.instruction(&Instruction::I64Load(MemArg {
        offset: 8,
        align: 0,
        memory_index: 0,
    }));
    f.instruction(&Instruction::Call(base + RT_RELEASE));
    f.instruction(&Instruction::I32Const(16));
    f.instruction(&Instruction::LocalSet(5));
    f.instruction(&Instruction::End);

    // if size != 0 { rt_free(addr, size) }
    f.instruction(&Instruction::LocalGet(5));
    f.instruction(&Instruction::If(empty));
    f.instruction(&Instruction::LocalGet(1));
    f.instruction(&Instruction::LocalGet(5));
    f.instruction(&Instruction::Call(base + RT_FREE));
    f.instruction(&Instruction::End);
    f.instruction(&Instruction::End);
    f
}

#[cfg(test)]
mod alloc_free_tests {
    use super::{emit_alloc, emit_free};
    use wasm_encoder::{
        CodeSection, ConstExpr, ExportKind, ExportSection, FunctionSection, GlobalSection,
        GlobalType, MemorySection, MemoryType, Module, TypeSection, ValType,
    };
    use wasmtime::{Engine, Instance, Store};

    // Build a minimal module: global 0 = __heap_ptr (init 1024), global 1 =
    // free-list head (init 0), func 0 = rt_alloc, func 1 = rt_free. Drive them
    // directly to verify free + same-size reuse.
    fn build() -> Vec<u8> {
        let fl = 1u32; // free-list head global index
        let mut types = TypeSection::new();
        types.ty().function([ValType::I32], [ValType::I32]); // alloc: (size)->ptr
        types.ty().function([ValType::I32, ValType::I32], []); // free: (ptr,size)->()
        let mut funcs = FunctionSection::new();
        funcs.function(0);
        funcs.function(1);
        let mut mem = MemorySection::new();
        mem.memory(MemoryType {
            minimum: 1,
            maximum: None,
            memory64: false,
            shared: false,
            page_size_log2: None,
        });
        let i32mut = GlobalType {
            val_type: ValType::I32,
            mutable: true,
            shared: false,
        };
        let live = 2u32; // live-object counter global index
        let bucket_base = 1024u32; // zero-init bucket-head region
        let heap_init = bucket_base + super::FREE_BUCKET_REGION_BYTES; // heap bump starts past it
        let mut globals = GlobalSection::new();
        globals.global(i32mut, &ConstExpr::i32_const(heap_init as i32)); // __heap_ptr
        globals.global(i32mut, &ConstExpr::i32_const(0)); // free-list head
        globals.global(i32mut, &ConstExpr::i32_const(0)); // live-object counter
        let mut exports = ExportSection::new();
        exports.export("alloc", ExportKind::Func, 0);
        exports.export("free", ExportKind::Func, 1);
        exports.export("heap", ExportKind::Global, 0);
        let mut code = CodeSection::new();
        // No imports in this fixture module — an empty remap makes the
        // OOM trap-report degrade to a bare `unreachable`, which is fine.
        code.function(&emit_alloc(fl, live, bucket_base, &[]));
        code.function(&emit_free(fl, live, bucket_base, &[]));
        let mut m = Module::new();
        m.section(&types);
        m.section(&funcs);
        m.section(&mem);
        m.section(&globals);
        m.section(&exports);
        m.section(&code);
        m.finish()
    }

    fn inst() -> (Store<()>, Instance) {
        let engine = Engine::default();
        let module = wasmtime::Module::new(&engine, build()).expect("module builds");
        let mut store = Store::new(&engine, ());
        let instance = Instance::new(&mut store, &module, &[]).expect("instantiates");
        (store, instance)
    }

    #[test]
    fn alloc_bumps_when_freelist_empty() {
        let (mut store, instance) = inst();
        let alloc = instance
            .get_typed_func::<i32, i32>(&mut store, "alloc")
            .unwrap();
        let a = alloc.call(&mut store, 16).unwrap();
        let b = alloc.call(&mut store, 16).unwrap();
        // heap starts past the bucket region (1024 + FREE_BUCKET_REGION_BYTES);
        // the logical pointer is base+8 (rc prefix, plan 113)
        let heap_init = 1024 + super::FREE_BUCKET_REGION_BYTES as i32;
        assert_eq!(
            a,
            heap_init + 8,
            "first alloc is heap start + 8-byte rc prefix"
        );
        assert!(b > a, "second alloc bumps past the first (no free yet)");
    }

    #[test]
    fn free_then_alloc_reuses_same_size_block() {
        let (mut store, instance) = inst();
        let alloc = instance
            .get_typed_func::<i32, i32>(&mut store, "alloc")
            .unwrap();
        let free = instance
            .get_typed_func::<(i32, i32), ()>(&mut store, "free")
            .unwrap();
        let a = alloc.call(&mut store, 16).unwrap();
        free.call(&mut store, (a, 16)).unwrap();
        let b = alloc.call(&mut store, 16).unwrap();
        assert_eq!(b, a, "freed block is reused by the next same-size alloc");
        // free list is now empty again → next alloc bumps a fresh block
        let c = alloc.call(&mut store, 16).unwrap();
        assert!(
            c > a,
            "after the freelist drains, alloc bumps a fresh block"
        );
    }

    #[test]
    fn two_frees_reuse_lifo() {
        let (mut store, instance) = inst();
        let alloc = instance
            .get_typed_func::<i32, i32>(&mut store, "alloc")
            .unwrap();
        let free = instance
            .get_typed_func::<(i32, i32), ()>(&mut store, "free")
            .unwrap();
        let a = alloc.call(&mut store, 16).unwrap();
        let b = alloc.call(&mut store, 16).unwrap();
        free.call(&mut store, (a, 16)).unwrap();
        free.call(&mut store, (b, 16)).unwrap();
        // LIFO: last freed (b) is reused first
        assert_eq!(alloc.call(&mut store, 16).unwrap(), b);
        assert_eq!(alloc.call(&mut store, 16).unwrap(), a);
    }

    #[test]
    fn alloc_reuses_only_exact_size_freed_block() {
        let (mut store, instance) = inst();
        let alloc = instance
            .get_typed_func::<i32, i32>(&mut store, "alloc")
            .unwrap();
        let free = instance
            .get_typed_func::<(i32, i32), ()>(&mut store, "free")
            .unwrap();
        // A freed 16-byte block is NOT reused for a larger (64) request…
        let a = alloc.call(&mut store, 16).unwrap();
        free.call(&mut store, (a, 16)).unwrap();
        let big = alloc.call(&mut store, 64).unwrap();
        assert!(big > a, "larger request bumps past a too-small freed block");
        // …nor for a smaller (8) request: exact-fit means a small request must
        // never grab a larger block (which would then be lost at its smaller
        // size). This is the property that keeps mixed-size loops tight.
        let b = alloc.call(&mut store, 32).unwrap();
        free.call(&mut store, (b, 32)).unwrap();
        let small = alloc.call(&mut store, 8).unwrap();
        assert!(
            small > b,
            "smaller request bumps rather than grabbing a larger freed block"
        );
    }
}
