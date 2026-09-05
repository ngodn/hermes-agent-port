# Native tool-name recovery

The native loop now applies Python's tool-name recovery before rejecting an
unknown name. It checks lowercase and separator normalization, CamelCase,
two rounds of tool-suffix stripping, XML attribute fragments, and finally
SequenceMatcher similarity at the reference 0.7 cutoff.

The fuzzy matcher follows CPython's longest-match tie order and default autojunk
behavior, with the query as sequence B. Fuzzy score ties choose the largest name,
matching get_close_matches. Exact candidate-set collisions have a documented
boundary: Python's set iteration varies with its hash seed; Rust chooses lexical
order among the same candidates. Unambiguous matches have no such difference.

Native HTTP response handling repairs names before deterministic missing-ID
construction. The loop also repairs names returned by other ChatModel
implementations before execution and keeps assistant replay names aligned.

Evidence:

- 143 fixtures execute the actual Python repair function, including difflib.
  They cover class/suffix variants, separators, XML fragments, generated typos,
  fuzzy ties and repeated strings spanning the autojunk threshold. Regeneration
  passes with Python 3.12.13 under two different hash seeds.
- The main/summary HTTP fixture starts each scenario with an ID-less
  CurrentTimeTool_tool call. It verifies that the calls execute successfully,
  continue across tool rounds, and finish under the configured turn policy.
- Workspace: 1,190 tests passed, one existing bridge test ignored.

Remaining invalid-call work includes the all-invalid batch strike counter and
termination policy, along with full mixed-batch middleware integration. The
Python tool registry's legacy aliases are separate from this spelling-repair
policy and still need integration with the native tool runtime.

Invalid-name retry follow-up: the native loop stops after three consecutive
all-invalid batches, using Python's final response and 80-character name preview.
A mixed or fully valid batch resets the strike counter. Valid calls in a mixed
batch still execute. The third invalid batch exits before appending/executing
its calls and does not enter the budget-summary fallback. Inline sequence tests
cover termination, reset by a mixed batch, successful execution counts and final
stop delivery. Workspace: 1,191 tests passed, one existing bridge test ignored.

Python returns explicit partial/completed metadata and persists a closing
assistant message here. Native AgentClient currently expresses the unsuccessful
partial stop as an error plus final text/stop events; full partial-result metadata
and incremental persistence are not implemented. Further source inspection also
confirmed that the wider Python loop normalizes blank/dict argument values before
its execution guard, a stage still missing from the native argument pipeline.
