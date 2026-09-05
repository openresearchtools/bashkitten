# Real model run results

These are actual BashKitten session processes using the configured OpenAI subscription. A task passes only after independent checks. This is not a claim of complete Pi parity.

| Task | Model | Status | Session |
| --- | --- | --- | --- |
| csv-inventory-repair | openai-codex/gpt-5.6-luna | passed | `01a07328-1643-79a2-9855-0735163b0770` |
| config-parser-refactor | openai-codex/gpt-5.6-sol | passed | `01a07328-16a4-7f71-8105-7ed247e71847` |

## csv-inventory-repair

Working folder: `/tmp/bashkitten-live-h9dxl1m1/csv-inventory-repair`

Tools observed: ls, find, grep, read, read, read, read, edit, write, bash, bash.
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

Working folder: `/tmp/bashkitten-live-h9dxl1m1/config-parser-refactor`

Tools observed: ls, read, read, find, read, read, ls, write, write, write, bash, edit, bash.
Supplied tests unchanged: True.

supplied checks (exit 0):
```text
test_basic (test_parser.ParserTests.test_basic) ... ok
test_comments (test_parser.ParserTests.test_comments) ... ok
test_empty (test_parser.ParserTests.test_empty) ... ok
test_bom_crlf_comments_and_blank_lines (test_parser_edge_cases.ParserEdgeCaseTests.test_bom_crlf_comments_and_blank_lines) ... ok
test_empty_key_reports_original_line_number (test_parser_edge_cases.ParserEdgeCaseTests.test_empty_key_reports_original_line_number) ... ok
test_empty_value_and_last_duplicate_wins (test_parser_edge_cases.ParserEdgeCaseTests.test_empty_value_and_last_duplicate_wins) ... ok
test_missing_equals_reports_original_line_number (test_parser_edge_cases.ParserEdgeCaseTests.test_missing_equals_reports_original_line_number) ... ok
test_only_one_initial_bom_is_removed (test_parser_edge_cases.ParserEdgeCaseTests.test_only_one_initial_bom_is_removed) ... ok
test_values_keep_hashes_and_additional_equals (test_parser_edge_cases.ParserEdgeCaseTests.test_values_keep_hashes_and_additional_equals) ... ok

----------------------------------------------------------------------
Ran 9 tests in 0.000s

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

## codex-transport-and-cache

Both real sessions used the new native auto transport with no fallback diagnostics. Luna reported 9,216 cached input tokens; Sol reported 8,576. The corresponding JSON contains per-task transport evidence. Both worker processes completed. These runs verify normal transport/tool/replay behavior; they do not certify all error, proxy, or lifecycle edge cases.
