# Substrate walls — `hew.dag` (runtime workflow DAG handler)

This is the record of substrate gaps that shaped this library. Every
reproduction below was last re-checked against the pinned compiler,
`v0.6.0-rc2-dev.107+84c34d2bd` (see [`../docs/toolchain.md`](../docs/toolchain.md)),
and each one states what that compiler does today. Re-check them when the pin
moves: a wall that has lifted is a design constraint that no longer earns its
keep.

The library is **fully working** for everything it exposes: inline tests and
runnable examples cover the pure routing engine (spec loading, validation,
topological sort, `run_text` / `route_with`), which imports and lowers cleanly
across the `hew.dag` package boundary; live-actor routing works in a single
compilation unit (see `examples/actor_pipeline.hew`).

The API splits routing into a pure, importable engine plus a consumer-owned
actor driver. Two of the three constraints that dictated that split have since
lifted; the third has become worse, and is now the reason the split must stay.

---

## WALL A — an explicit `Vec<LocalPid<Actor>>` annotation inside an imported module does not lower

Repro — a library module that annotates a vector of its own actor's pids:

```hew
// m.hew
pub actor Worker { let b: i64; receive fn go(x: i64) -> i64 { x + b } }
pub fn count_pids() -> i64 {
    let v: Vec<LocalPid<Worker>> = Vec.new();
    v.push(spawn Worker(b: 1));
    v.len()
}
```

```hew
// consumer.hew
import m;
fn main() { println(m.count_pids()); }
```

`hew run consumer.hew`:

```
error: E_NOT_YET_IMPLEMENTED: MIR lowering for typed produced value published
into non-congruent storage is not implemented yet
 4 |     v.push(spawn Worker(b: 1));
   |     ^
  = help: site s10 has type Vec<LocalPid<m.Worker>>, but Local(1) has type
    Vec<LocalPid<Worker>>
```

**Why it walls:** the annotation is written inside the module, where the actor
is `Worker`; the value the module produces is typed against the importing
unit's view, where it is `m.Worker`. The two spellings name the same actor and
are not treated as congruent, so the store has no lowering.

**Scope — this is the narrow remnant of a much larger wall.** All of the
following now work, and none of them did when this library was designed:

- importing a module that contains an actor, whether or not the consumer uses
  it;
- a consumer spawning and awaiting an imported actor
  (`spawn m.Worker(b: 1)`, `await w.go(41)`);
- a consumer holding `Vec<LocalPid<m.Worker>>` — the annotation is congruent
  when it is written from the importing side;
- the same vector built inside the module with the annotation *omitted*
  (`let v = [spawn Worker(b: 1)];`), which infers the module-qualified type and
  lowers;
- a module owning an actor privately and awaiting it behind a plain function.

**Workaround:** omit the type annotation, or write the vector on the consumer
side. This library does neither, because it ships no actor at all — a decision
made when the wall was total, and left in place because the routing seam it
produced is the right shape independently (see the Verdict).

**Ideal version needs:** a module-qualified and an unqualified reference to the
same actor to be one type at the MIR boundary.

---

## WALL B — `await` inside a closure compiles and returns a wrong answer

**This is the wall that dictates the routing architecture today, and it is a
correctness defect rather than a refusal.**

Repro — pass a closure that awaits an actor as a `fn` argument:

```hew
actor Stage { let b: i64; receive fn run(x: i64) -> i64 { x + b } }
fn apply(f: fn(i64) -> i64) -> i64 { f(10) }
fn main() {
    let s = spawn Stage(b: 5);
    let r = apply(|x: i64| -> i64 {
        let v = await s.run(x);
        match v { .Ok(n) => n, .Err(_) => -1 }
    });
    println(r);
}
```

`hew check` reports `OK`. `hew run` exits 0 and prints a different large
integer on each run — `4379154480`, `4347680800`, `38130516160` on three
consecutive runs — where the answer is `15`. No diagnostic is emitted at any
stage.

Two controls isolate it. A closure capturing an `i64` and passed the same way
prints `15`. The same `await`, written at statement level instead of inside a
closure, prints `15`. Only the combination is wrong, and the values look like
addresses rather than arithmetic.

**Why it walls:** an awaited actor call inside a closure body reaches the
consumer as an unmodelled value. Previously this was refused at MIR lowering;
it now lowers, and what the closure returns is not the reply.

**Workaround (in use):** the actor driver loop is open-coded with the `await` at
statement level inside `main` or a plain function, never inside a closure.
`route_with`'s `fn` dispatch is therefore reserved for **pure** per-node logic.
That restriction used to be about what would compile. It is now about what
computes the right answer, which is why it is stated in `dag.hew`'s doc comments
as a rule rather than a limitation.

**Ideal version needs:** an awaited actor call inside a closure to either lower
correctly or fail closed. Silently returning a wrong `i64` is the one outcome a
caller cannot defend against.

---

## WALL C — lifted: an imported `machine` no longer collides with a consumer's actor

A library module containing a `machine`, imported by a consumer that also
defines and spawns its own actor, used to trip the fail-closed D10 gate
(`Named/user type m.Coord reached the LLVM emitter`). It no longer does.

The original reproduction cannot be run verbatim: a machine's transition target
is a machine-qualified state constructor (`Coord.B`), and the contextual `.B`
spelling the old repro used is now rejected with `E_CONTEXT_VARIANT_NO_TYPE`.
Written against the current grammar:

```hew
// m.hew
pub machine Coord {
    events { Go; }
    state A;
    state B;
    on Go: A => B { Coord.B }
    default { state }
}
pub fn pure_add(a: i64, b: i64) -> i64 { a + b }
```

```hew
// consumer.hew
import m;
actor Local { let b: i64; receive fn go(x: i64) -> i64 { x + b } }
fn main() {
    var coord = m.Coord.A;
    coord.step(.Go);
    println(coord.state_name());
    let a = spawn Local(b: 10);
    match await a.go(m.pure_add(1, 2)) {
        .Ok(x) => println(x),
        .Err(_) => println("actor stopped"),
    }
}
```

This runs and prints `B` then `13`: the consumer drives the imported machine
and its own actor in one unit.

**Consequence for this library.** The coordinator lifecycle here is a plain
`enum LifecycleState` with a `lifecycle_for(spec)` stepping function and a
`state_name` helper, written that way because a `machine` could not cross the
import boundary alongside an actor. That constraint is gone, so the lifecycle
could become a `machine` — the more explicit transition model — whenever the
API is next revised. The enum is not wrong, it is merely no longer forced.

---

## What was NOT a wall (substrate strengths this library leans on)

- **`Vec<LocalPid<Actor>>` and asking actors by index** — spawn a vector of live
  node actors and `await pid.process(x)` in topological order. This carries the
  actor-routing example.
- **`await` at statement level inside a `#[test]`** — the routing tests assert
  over live results.
- **Runtime string parsing** with `.split` / `.trim` / `string.to_int` — the spec
  is loaded from text at runtime, not built in code.
- **Flat (Copy) `Vec<record>` (`NodeSpec`, `Edge`)** — index, `.field` read,
  `len`, `push`, `set`. The structure-of-arrays spec is built on this.
- **`fn(i64, i64) -> i64` as a call-site argument**, including a read-only inline
  closure — `route_with`'s dispatch seam.
- **Cross-package `import hew.dag`** resolving through the `hew/dag -> ../dag`
  mirror symlink, with the module file named after the final path segment
  (`dag.hew`).
- **`Result<T, E>` with a payload-carrying error enum** and `match` — the
  fail-closed cyclic/empty rejection path.

---

## Verdict

A runtime-loaded DAG router is fully expressible in Hew. The pure routing engine
is importable and lowers cleanly; live-actor routing works in a single
compilation unit.

The split into an importable pure engine plus consumer-owned actors was
originally forced by three cross-module lowering gaps. Two of them have lifted.
The split stays because of the third and because it is the right shape anyway:
`route_with`'s dispatch seam takes a `fn`, and an awaited actor call inside a
closure currently produces a wrong answer with no diagnostic, so a library that
invited callers to await inside that seam would be inviting silent corruption.
Keeping the seam pure and the `await` at statement level in the consumer's own
loop is both what works and what a reader can check.
