//! Manual, test-only capture of published canonical bytecode.
//! Included by run_dump.py in a disposable worktree; not a production module.
use crate::engine::api::{compile::Compilation, Runtime};
use crate::engine::heap::{BytecodeConstant, FunctionBytecodeId, Heap};
use crate::engine::value::JsString;
use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::path::PathBuf;

const TARGETS: [&str; 4] = ["am3", "project", "lin_solve", "advect"];

fn walk(
    heap: &Heap,
    id: FunctionBytecodeId,
    path: &str,
    out: &mut File,
    counts: &mut [usize; 4],
) -> io::Result<()> {
    let data = heap.function_bytecode(id).map_err(|error| {
        io::Error::other(format!("cannot read published function {path}: {error:?}"))
    })?;
    let target = TARGETS.iter().position(|name| {
        data.func_name
            .as_ref()
            .is_some_and(|actual| actual == &JsString::from_static(*name))
    });
    if let Some(target) = target {
        counts[target] += 1;
        writeln!(out, "FUNCTION\t{}\t{}\t{:?}", TARGETS[target], path, id)?;
        writeln!(out, "METADATA\t{:?}", data.metadata)?;
        writeln!(out, "ARGUMENTS\t{:?}", data.argument_definitions)?;
        writeln!(out, "LOCALS\t{:?}", data.local_definitions)?;
        writeln!(out, "CLOSURES\t{:?}", data.closure_variables)?;
        for (index, value) in data.constants.iter().enumerate() {
            writeln!(out, "CONSTANT\t{index}\t{value:?}")?;
        }
        for (pc, instruction) in data.code.iter().enumerate() {
            let stack = instruction.stack_contract();
            let control = instruction.control_effect();
            writeln!(
                out,
                "OP\t{pc}\t{instruction:?}\tpop={}\tpush={}\ttarget={:?}\tends_block={}",
                stack.popped, stack.pushed, control.target(), control.ends_block(),
            )?;
        }
        writeln!(out, "END_FUNCTION\t{}", TARGETS[target])?;
    }
    // The root is held for this entire walk; children are owned constant edges.
    // No retain/release or mutable runtime borrow is needed for inspection.
    for (index, constant) in data.constants.iter().enumerate() {
        if let BytecodeConstant::Function(child) = constant {
            walk(heap, *child, &format!("{path}/constant[{index}]"), out, counts)?;
        }
    }
    Ok(())
}

#[test]
#[ignore = "manual external-source bytecode capture; use docs/performance/probes/run_dump.py"]
fn dump_numeric_spans() {
    let source = PathBuf::from(std::env::var_os("OXIDE_DUMP_SOURCE").expect("OXIDE_DUMP_SOURCE"));
    let output = PathBuf::from(std::env::var_os("OXIDE_DUMP_OUTPUT").expect("OXIDE_DUMP_OUTPUT"));
    let mut counts = [0_usize; 4];
    for name in ["crypto", "navier-stokes"] {
        let path = source.join("v8-v7").join(format!("{name}.js"));
        let text = std::fs::read_to_string(&path).expect("read complete pinned source");
        let runtime = Runtime::new();
        let context = runtime.new_context();
        let compilation = runtime
            .compile_in_realm(context.realm, &text, &format!("v8-v7/{name}.js"))
            .expect("compile and publish complete source");
        let root = match compilation {
            Compilation::Published(root) => root,
            Compilation::Throw(value) => {
                runtime.release_jsvalue(value).expect("release compilation exception");
                panic!("source compilation threw: {name}");
            }
        };
        let mut out = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(output.join(format!("{name}.canonical.txt")))
            .expect("create fresh canonical output");
        writeln!(out, "FORMAT\toxide-published-canonical-v1").unwrap();
        writeln!(out, "SOURCE\tv8-v7/{name}.js").unwrap();
        {
            let state = runtime.0.state.borrow();
            walk(&state.heap, root.bytecode_id(), name, &mut out, &mut counts)
                .expect("walk published bytecode while root is alive");
        }
        out.sync_all().unwrap();
        // Neither the script nor any benchmark function has been executed.
    }
    assert_eq!(counts, [1, 1, 1, 1], "missing or ambiguous target function");
    let mut out = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(output.join("target-counts.tsv"))
        .expect("create target count receipt");
    for (name, count) in TARGETS.iter().zip(counts) {
        writeln!(out, "{name}\t{count}").unwrap();
    }
    out.sync_all().unwrap();
}
