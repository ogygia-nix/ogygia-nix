## Code Review Rules

### Clean room
- This crate re-implements Nix's evaluator without reading Nix's source or the
  source of any library Nix uses. Never link to, quote or paraphrase that
  source, or describe how Nix's code works internally.
- Describe a difference from Nix only as Nix input, the result Nix gives and
  the result this code gives.

### What pins behaviour
- Two things compare this crate with Nix: the equivalence cases under
  `tests/cases`, and a differential fuzzer (`ogygia-nix-eval-fuzz`) that
  evaluates random input with both.
- Behaviour they cover is the specification. Behaviour they don't cover is
  not promised, and a change may alter it freely.
- The fuzzer can't reach impure behaviour (the clock, the environment, the
  filesystem outside its input), error messages, fetching over the network,
  or writing lock files. Only equivalence cases or other tests cover those.

### What to review
- Work out from the diff which behaviour the change touches and whether a
  case or the fuzzer covers it.
- A change whose new behaviour is pinned by an equivalence case that fails
  before it and passes after it needs no review of that behaviour. Don't ask
  for more tests, or for it to match Nix in cases it doesn't test.
- A refactor needs no review of behaviour the cases or the fuzzer cover.
  Don't ask it to preserve behaviour nothing covers.
- Behaviour the change touches that neither covers, such as remote fetching,
  does need its correctness reviewed, and a test where one is possible.
- Otherwise review for style: duplicated logic, code that could be simpler,
  refactors the change makes possible, and naming.
