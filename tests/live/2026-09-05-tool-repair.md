# Real model run results

These are actual BashKitten session processes using the configured OpenAI subscription. A task passes only after independent checks. This is not a claim of complete Pi parity.

| Task | Model | Status | Session |
| --- | --- | --- | --- |
| csv-inventory-repair | openai-codex/gpt-5.6-luna | passed | `01a0729e-e473-7652-bbb6-2cfa7ccb8387` |
| config-parser-refactor | openai-codex/gpt-5.6-sol | passed | `01a0729e-e4bc-7ce2-b20a-b4e4769e11d7` |

## csv-inventory-repair

Working folder: `/tmp/bashkitten-live-bt6cpca5/csv-inventory-repair`

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

Working folder: `/tmp/bashkitten-live-bt6cpca5/config-parser-refactor`

Tools observed: ls, read, read, find, read, read, ls, write, write, write, bash, edit, bash.
Supplied tests unchanged: True.

supplied checks (exit 0):
```text
test_basic (test_parser.ParserTests.test_basic) ... ok
test_comments (test_parser.ParserTests.test_comments) ... ok
test_empty (test_parser.ParserTests.test_empty) ... ok
test_bom_crlf_comments_and_blank_lines (test_parser_edge_cases.ParserEdgeCaseTests.test_bom_crlf_comments_and_blank_lines) ... ok
test_empty_key_reports_original_line_number (test_parser_edge_cases.ParserEdgeCaseTests.test_empty_key_reports_original_line_number) ... ok
test_empty_value_and_last_duplicate (test_parser_edge_cases.ParserEdgeCaseTests.test_empty_value_and_last_duplicate) ... ok
test_missing_equals_reports_original_line_number (test_parser_edge_cases.ParserEdgeCaseTests.test_missing_equals_reports_original_line_number) ... ok
test_only_one_initial_bom_is_removed (test_parser_edge_cases.ParserEdgeCaseTests.test_only_one_initial_bom_is_removed) ... ok
test_value_keeps_hashes_and_additional_equals (test_parser_edge_cases.ParserEdgeCaseTests.test_value_keeps_hashes_and_additional_equals) ... ok

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
