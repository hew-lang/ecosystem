# hew.dag

Runtime-loaded, fail-closed directed acyclic graph parsing and value routing.
Malformed specs, unknown operations, bad node references, and cycles are typed
`DagError` values.

```hew
import hew.dag;

fn main() {
    match dag.run_text("add:1 multiply:10 | 0->1", 3) {
        .Ok(report) => println(f"sink = {dag.sink_output(report)}"),
        .Err(failure) => println(dag.describe(dag.error_of(failure))),
    }
}
```

Run `hew run dag/examples/linear_pipeline.hew` from the ecosystem repository
root for a complete example.

## API surface

**Types**

- `NodeOp` — `Add | Multiply | Square | Identity`, the operation a node applies.
- `NodeSpec { id, op_code, operand }` — one node's compiled operation.
- `Edge { src, dst }` — a directed dependency (`dst` runs after `src`).
- `DagSpec { num_nodes, nodes, edges }` — a compiled graph.
- `RunReport { outputs, order, num_nodes }` — the outcome of a run.
- `RunFailure { err, stopped_at }` — a run failure with the node it stopped at.
- `DagError` — `Empty | Cyclic | UnknownNode(i64) | Malformed(i64) | UnknownOperation(i64)`.
- `LifecycleState` — the spec's readiness (`lifecycle_for`/`state_name`).

**Building and parsing specs**

- `new_spec() -> DagSpec` / `add_node(spec, op, operand) -> DagSpec` /
  `add_edge(spec, src, dst) -> Result<DagSpec, DagError>` — build a spec
  programmatically.
- `parse_spec(text: string) -> Result<DagSpec, DagError>` — parse the
  `op:operand ... | src->dst ; ...` text grammar (see the module doc comment
  in `dag.hew`).

**Routing**

- `run_text(text, initial) -> Result<RunReport, RunFailure>` — parse and
  route in one call, using the library's own `apply_op` kernel.
- `route_with(spec, initial, dispatch: fn(i64, i64) -> i64) -> Result<RunReport, RunFailure>` —
  route through a caller-supplied per-node dispatch function.
- `apply_op(op, x, operand) -> i64` — the per-node operation kernel
  (`add`/`multiply`/`square`/`identity`).
- `topo_order(spec) -> Result<Vec<i64>, DagError>` / `validate(spec) -> Result<i64, DagError>` /
  `is_acyclic(spec) -> Result<bool, DagError>` — topological sort and
  validation, refusing cyclic specs.

**Inspecting results**

- `node_count`, `edge_count`, `edge_src`, `edge_dst`, `node_at` — read a
  `DagSpec`.
- `output_at(report, id)`, `sink_output(report)` — read a `RunReport`.
- `error_of(failure) -> DagError`, `describe(error) -> string`,
  `is_cyclic(error) -> bool` — inspect and render a `DagError`.
- `lifecycle_for(spec) -> LifecycleState`, `state_name(state) -> string` —
  a spec's lifecycle state before routing.

For routing through one live actor per node instead of a plain dispatch
function, see `dag/examples/actor_pipeline.hew` — the driver is open-coded
in the consumer (see `dag.hew`'s module doc comment and `WALLS.md` for why).
