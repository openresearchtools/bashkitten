# Real model run results

These are actual BashKitten session processes using the configured OpenAI subscription. A task passes only after independent checks. This is not a claim of complete Pi parity.

| Task | Model | Status | Session |
| --- | --- | --- | --- |
| csv-inventory-repair | openai-codex/gpt-5.6-luna | passed | `01a07221-8ce5-7733-b0cc-de9a2de2079c` |
| config-parser-refactor | openai-codex/gpt-5.6-sol | passed | `01a07221-8d2d-7bf2-8532-634921a898fa` |

## csv-inventory-repair

Working folder: `/tmp/bashkitten-live-w5q5ydxd/csv-inventory-repair`

Tools observed: ls, find, grep, read, read, read, read, edit, write, bash, bash, read, read, bash, write.
Supplied tests unchanged: True.

supplied checks (exit 0):
```text
test_accumulate (test_inventory.InventoryTests.test_accumulate) ... ok
test_empty (test_inventory.InventoryTests.test_empty) ... ok
test_invalid (test_inventory.InventoryTests.test_invalid) ... ok

----------------------------------------------------------------------
Ran 3 tests in 0.000s

OK
```

independent checks (exit 0):
```text
test_bad_value (test_independent.IndependentTests.test_bad_value) ... ok
test_generator (test_independent.IndependentTests.test_generator) ... ok
test_separate_buckets_and_signed_integers (test_independent.IndependentTests.test_separate_buckets_and_signed_integers) ... ok

----------------------------------------------------------------------
Ran 3 tests in 0.000s

OK
```

## config-parser-refactor

Working folder: `/tmp/bashkitten-live-w5q5ydxd/config-parser-refactor`

Tools observed: ls, read, read, find, read, read, ls, write, write, write, bash, bash, edit, bash.
Supplied tests unchanged: True.

supplied checks (exit 0):
```text
test_basic (test_parser.ParserTests.test_basic) ... ok
test_comments (test_parser.ParserTests.test_comments) ... ok
test_empty (test_parser.ParserTests.test_empty) ... ok
test_empty_trimmed_key_reports_source_line (test_parser_edge_cases.ParserEdgeCaseTests.test_empty_trimmed_key_reports_source_line) ... ok
test_empty_value_is_allowed (test_parser_edge_cases.ParserEdgeCaseTests.test_empty_value_is_allowed) ... ok
test_indented_comments_and_whitespace_only_lines_are_skipped (test_parser_edge_cases.ParserEdgeCaseTests.test_indented_comments_and_whitespace_only_lines_are_skipped) ... ok
test_initial_bom_and_crlf (test_parser_edge_cases.ParserEdgeCaseTests.test_initial_bom_and_crlf) ... ok
test_last_duplicate_key_wins (test_parser_edge_cases.ParserEdgeCaseTests.test_last_duplicate_key_wins) ... ok
test_missing_separator_reports_source_line (test_parser_edge_cases.ParserEdgeCaseTests.test_missing_separator_reports_source_line) ... ok
test_only_one_initial_bom_is_removed (test_parser_edge_cases.ParserEdgeCaseTests.test_only_one_initial_bom_is_removed) ... ok
test_value_keeps_hashes_and_additional_equals (test_parser_edge_cases.ParserEdgeCaseTests.test_value_keeps_hashes_and_additional_equals) ... ok

----------------------------------------------------------------------
Ran 11 tests in 0.000s

OK
```

independent checks (exit 0):
```text
test_bom_crlf_duplicate_equals (test_independent.IndependentTests.test_bom_crlf_duplicate_equals) ... ok
test_comments_and_value (test_independent.IndependentTests.test_comments_and_value) ... ok
test_empty_key (test_independent.IndependentTests.test_empty_key) ... ok
test_error_line (test_independent.IndependentTests.test_error_line) ... ok

----------------------------------------------------------------------
Ran 4 tests in 0.000s

OK
```

## real-compaction-and-resume

Live summary/rotation passed: True. Segments: ['000001.jsonl', '000002.jsonl']. Old segment unchanged: True. Error: None. Used Pi's configurable keepRecentTokens=1000 for this small task and restored the test configuration afterward.

## real-cancellation-and-resume

Web Pause stopped the real bash command before its completion marker (passed: True); the aborted tool result is persisted. A new worker resumed from that history, read the existing files, wrote the expected resumed.md, and independently verified the cancelled command had not completed (passed: True).

## post-compaction-model-switch-and-image

Astra continued the compacted Luna session after a worker restart and model change, read the 2400×1600 image through read, and returned the expected two labels, panel count and sum (passed: True). Code tests are checked independently in the inventory task above. This verifies image delivery/replay, not Pi's still-missing image resize parity.

## automatic-project-instructions-before-repair

Before repair: requested answer created: True; mandatory artifact specified only in repository AGENTS.md created: False. The worker still uses a shortened prompt without automatic project-instruction loading. This is a confirmed parity failure, now being repaired.

## automatic-project-instructions-after-repair

Fresh Luna session created answer.txt with the requested value and instruction-proof.txt with the value specified only in AGENTS.md (passed: True). The actual-worker HTTP fixture also confirms unchanged system prompt, non-message fields and message prefixes over two turns and a worker restart, with earlier JSONL bytes intact. A subsequent real worker restart read both files and wrote the independently checked restart-proof.txt. The provider reported 1,536 cached input tokens during that restarted turn.

## Observed failure: long session socket path

Worker exited with session control socket was not created; no provider request was sent.

Retain an open parent directory through /proc/self/fd address resolution; bind before worker publication and refuse duplicate live workers.

Dedicated long-path/duplicate-worker runtime test passed; the real model sessions above run under the same long data directory.

## Observed failure: sidebar scroll and narrow window

Long sidebar moved the page/header; narrow windows hid the sidebar entirely.

Constrain grid/flex heights, retain the fixed header and list scroll position, keep a compact sidebar at narrow widths.

Wide-window independent scrolling verified in browser; narrow-width recheck pending.
