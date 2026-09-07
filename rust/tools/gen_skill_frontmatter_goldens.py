#!/usr/bin/env python3
"""Generate actual Python oracle golden fixtures for agent.skill_utils.parse_frontmatter.

This script exercises agent.skill_utils.parse_frontmatter across delimiter fences,
whitespace variants, CRLF line endings, UTF-8 byte order marks (BOM), malformed YAML
fallback recovery, nested metadata, duplicate keys, YAML anchors/merges, quoted versus
unquoted YAML 1.1 booleans and numbers, and unknown tag constructs.

Exclusions for JSON representability:
Non-string mapping keys (such as unquoted YAML 1.1 booleans or integers as mapping keys)
and YAML timestamps (which PyYAML loads as datetime objects) are intentionally excluded
from this initial corpus so all golden outputs remain strictly JSON-serializable.
"""

from __future__ import annotations

import json
from pathlib import Path
import sys
from typing import Any, Dict, List

REPO_ROOT = Path(__file__).resolve().parents[2]
if str(REPO_ROOT) not in sys.path:
    sys.path.insert(0, str(REPO_ROOT))

from agent.skill_utils import parse_frontmatter

OUT_PATH = REPO_ROOT / "rust" / "tools" / "skill-frontmatter-goldens.json"

INPUT_CASES: List[str] = [
    # 1. Baseline documents and edge fences
    "plain text without frontmatter",
    "# Skill Heading\n\nBody content in markdown.\n",
    "name: not-frontmatter\ndescription: missing fence",
    "---\nname: minimal\n---\nbody",
    "---\nname: minimal_with_trailing_newline\n---\nbody\n",
    "---\n---\nbody",
    "---\n---\n",
    "---\n---",
    "---",
    "------",
    "",
    "   \n\t\n",
    "---\nname: no_closing_fence\nbody continues here",
    "---\nname: no_body\n---\n",
    "---\nname: only_dashes_in_body\n---\n---",
    "---\nname: horizontal_rules\n---\nSection 1\n---\nSection 2\n---\n",
    "---\nname: codeblock\n---\n```yaml\n---\ninside: codeblock\n---\n```\n",

    # 2. Fences: Python whitespace variations
    "--- \nname: trailing_space_open\n---\nbody",
    "---   \nname: trailing_spaces_open\n---\nbody",
    "---\t\nname: trailing_tab_open\n---\nbody",
    "---\t \nname: trailing_mixed_open\n---\nbody",
    " ---\nname: leading_space_open\n---\nbody",
    "  ---\nname: leading_spaces_open\n---\nbody",
    "\t---\nname: leading_tab_open\n---\nbody",
    "\n---\nname: leading_newline_open\n---\nbody",
    "---\nname: trailing_space_close\n--- \nbody",
    "---\nname: trailing_spaces_close\n---   \nbody",
    "---\nname: trailing_tab_close\n---\t\nbody",
    "---\nname: trailing_mixed_close\n---\t \nbody",
    "---\nname: extra_dash_open\n---\nbody",
    "----\nname: extra_dash_open_4\n---\nbody",
    "-----\nname: extra_dash_open_5\n---\nbody",
    "---\nname: extra_dash_close_4\n----\nbody",
    "---\nname: trailing_chars_close\n---tail\nbody",
    "---\nname: greedy_whitespace\n---\n\n\nbody",
    "---\nname: greedy_spaces_lines\n---\n  \n\t\n\nbody",

    # 3. Fences: CRLF variations
    "---\r\nname: crlf_pure\r\n---\r\nbody\r\n",
    "---\r\nname: crlf_empty_body\r\n---\r\n",
    "---\r\n---\r\n",
    "---\r\n---\r\nbody\r\n",
    "---\r\nname: crlf_no_close_newline\r\n---",
    "---\r\nname: crlf_open_lf_close\n---\nbody",
    "---\nname: lf_open_crlf_close\r\n---\r\nbody",
    "---\r\nname: crlf_spaces_close\r\n---  \r\nbody\r\n",
    "---\r\nname: crlf_tabs_close\r\n---\t\r\nbody\r\n",
    "---\r\nname: crlf_multiline\r\ndescription: line1\r\nline2\r\n---\r\nbody\r\n",
    "---\r\nname: crlf_blank_lines_greedy\r\n---\r\n\r\n\r\nbody\r\n",
    "---\r\nname: crlf_mixed_newlines\ndescription: line1\r\n---\nbody\r\n",

    # 4. BOM (Byte Order Mark, U+FEFF)
    "\ufeff---\nname: bom_basic\n---\nbody",
    "\ufeff---\r\nname: bom_crlf\r\n---\r\nbody\r\n",
    "\ufeff\ufeff---\nname: double_bom\n---\nbody",
    "\ufeff\ufeff\ufeff---\nname: triple_bom\n---\nbody",
    "\ufeff# Heading\nPlain markdown with BOM",
    "\ufeff",
    "\ufeff---",
    "---\n\ufeffname: bom_in_key\n---\nbody",
    "---\nname: \ufeffbom_in_val\n---\nbody",
    "---\nname: test\n\ufeff---\nbody",
    "---\nname: test\n---\n\ufeffbody_with_bom",

    # 5. Malformed YAML fallback
    "---\nname: tab_indent\n\tdescription: has tab\n---\nbody",
    '---\nname: "unclosed double quote\nauthor: alice\n---\nbody',
    "---\nname: 'unclosed single quote\nauthor: bob\n---\nbody",
    "---\nname: bad_bracket\ntags: [unclosed, list\nauthor: charlie\n---\nbody",
    "---\nname: bad_brace\nconfig: {unclosed: brace\nauthor: dave\n---\nbody",
    "---\n[syntax_error\nname: colon_no_space\nauthor:eve\n---\nbody",
    "---\n[syntax_error\nurl: https://example.com:8080/path\n---\nbody",
    "---\n[syntax_error\nname: fallback_with_comments\n# comment line\nkey: value\n---\nbody",
    "---\n[syntax_error\nname: empty_val\nempty:\n---\nbody",
    "---\n[syntax_error\n: empty_key\nname: valid\n---\nbody",
    "---\n[syntax_error\n  padded_key  :  padded_val  \n---\nbody",
    "---\n[syntax_error\nno colon line here\nname: parsed\n---\nbody",
    "---\n[syntax_error\nkey: first\nkey: second\n---\nbody",
    "---\n- item1\n- item2\n---\nbody",
    "---\njust a scalar string\n---\nbody",
    "---\n42\n---\nbody",
    "---\n3.1415\n---\nbody",
    "---\ntrue\n---\nbody",
    "---\nnull\n---\nbody",
    "---\n   \t  \n---\nbody",

    # 6. Nested metadata
    "---\nname: hermes_skill\ndescription: Skill metadata\nmetadata:\n  hermes:\n    fallback_for_toolsets:\n      - browser\n      - code\n    requires_tools:\n      - bash\n      - web_search\n    session_platforms:\n      - linux\n      - macos\n    priority: 10\n    config:\n      active: true\n      retries: 3\n---\nbody",
    "---\nname: catalog\nitems:\n  - id: 1\n    label: first\n  - id: 2\n    label: second\n---\nbody",
    "---\nlevel1:\n  level2:\n    level3:\n      level4:\n        leaf: deep\n---\nbody",
    "---\nname: flow_style\nplatforms: [linux, macos, win32]\nsettings: {enabled: true, timeout: 30}\n---\nbody",
    "---\nname: complex\ntags: [a, b, c]\nruntimes:\n  - name: node\n    versions: [18, 20]\n  - name: python\n    versions: [3.11, 3.12]\n---\nbody",

    # 7. Duplicate keys
    "---\nname: first\nname: second\nkey: original\nkey: final\n---\nbody",
    "---\nmetadata:\n  attr: val1\n  attr: val2\n---\nbody",
    "---\ntools: [bash]\ntools: [edit, git]\n---\nbody",

    # 8. Aliases and Merges
    "---\ndefault_desc: &desc Standard description\nskill1:\n  name: one\n  desc: *desc\nskill2:\n  name: two\n  desc: *desc\n---\nbody",
    "---\nbase_tools: &tools\n  - bash\n  - edit\nskill_tools: *tools\n---\nbody",
    "---\nbase_config: &base\n  retries: 3\n  timeout: 30\n  env: prod\ncustom:\n  <<: *base\n  timeout: 60\n  extra: true\n---\nbody",
    "---\nbase_a: &base_a\n  key_a: 1\n  shared: from_a\nbase_b: &base_b\n  key_b: 2\n  shared: from_b\nmerged:\n  <<: [*base_a, *base_b]\n  final_key: 3\n---\nbody",
    "---\nsettings: &settings\n  debug: false\napp:\n  config:\n    <<: *settings\n    port: 8080\n---\nbody",

    # 9. Quoted vs unquoted YAML 1.1 booleans and numbers
    "---\nbool_true: true\nbool_True: True\nbool_TRUE: TRUE\nbool_false: false\nbool_False: False\nbool_FALSE: FALSE\nbool_yes: yes\nbool_Yes: Yes\nbool_YES: YES\nbool_no: no\nbool_No: No\nbool_NO: NO\nbool_on: on\nbool_On: On\nbool_ON: ON\nbool_off: off\nbool_Off: Off\nbool_OFF: OFF\nstr_y: y\nstr_Y: Y\nstr_n: n\nstr_N: N\n---\nbody",
    "---\nq_true: 'true'\nq_True: 'True'\nq_false: 'false'\nq_False: 'False'\nq_yes: 'yes'\nq_Yes: 'Yes'\nq_no: 'no'\nq_No: 'No'\nq_on: 'on'\nq_On: 'On'\nq_off: 'off'\nq_Off: 'Off'\nq_y: 'y'\nq_n: 'n'\ndq_true: \"true\"\ndq_false: \"false\"\ndq_yes: \"yes\"\ndq_no: \"no\"\ndq_on: \"on\"\ndq_off: \"off\"\n---\nbody",
    "---\nval_null: null\nval_Null: Null\nval_NULL: NULL\nval_tilde: ~\nq_null: 'null'\nq_tilde: '~'\n---\nbody",
    "---\nnum_int: 42\nnum_neg_int: -42\nnum_zero: 0\nnum_float: 3.14159\nnum_neg_float: -0.005\nnum_exp_pos: 1.5e+3\nnum_exp_neg: 2.5e-3\nnum_oct_11: 077\nnum_hex_lower: 0x2a\nnum_hex_upper: 0X2A\nnum_bin: 0b1010\n---\nbody",
    "---\nq_int: '42'\nq_neg_int: '-42'\nq_zero: '0'\nq_float: '3.14159'\nq_neg_float: '-0.005'\nq_exp: '1.5e+3'\nq_oct: '077'\nq_hex: '0x2a'\nq_bin: '0b1010'\n---\nbody",

    # 10. Unknown tags
    "---\ntagged_scalar: !custom_tag scalar_value\n---\nbody",
    "---\ntagged_double: !!unknown_type some_string\n---\nbody",
    "---\ntagged_map: !component\n  width: 100\n  height: 200\n---\nbody",
    "---\ntagged_seq: !items\n  - item_one\n  - item_two\n---\nbody",
    "---\nname: skill_with_tag\ntagged_val: !unknown_type hello\nauthor: alice\n---\nbody",
    "---\ntagged_verbatim: !<tag:example.com,2026:special> my_value\n---\nbody",
    "---\ntagged_int: !my_int 42\ntagged_bool: !my_bool true\n---\nbody",

    # 11. Multiple closing delimiters and body fences
    "---\nname: multi_fence\n---\nbody part 1\n---\nbody part 2\n---\nbody part 3\n",
    "---\nname: trailing_dashes_in_body\n---\nline 1\n---",
    "---\nname: fence_with_spaces_and_crlf\n---  \r\nbody\r\n",
    "---\r\nname: fence_crlf_multiple\r\n---\r\nbody 1\r\n---\r\nbody 2\r\n",
]


def generate_cases() -> List[Dict[str, Any]]:
    rows: List[Dict[str, Any]] = []
    for raw_input in INPUT_CASES:
        frontmatter, body = parse_frontmatter(raw_input)
        rows.append({
            "input": raw_input,
            "frontmatter": frontmatter,
            "body": body,
        })
    return rows


def main() -> None:
    cases = generate_cases()
    text = json.dumps(cases, indent=2) + "\n"

    if sys.argv[1:] == ["--check"]:
        if not OUT_PATH.exists():
            raise SystemExit(f"Golden file not found: {OUT_PATH}")
        expected = OUT_PATH.read_text(encoding="utf-8")
        if expected != text:
            raise SystemExit("Golden file out of sync. Run without --check to regenerate.")
    elif not sys.argv[1:]:
        OUT_PATH.write_text(text, encoding="utf-8")
    else:
        raise SystemExit("usage: gen_skill_frontmatter_goldens.py [--check]")

    print(
        f"Verified {len(cases)} skill frontmatter cases "
        "(exclusions: non-string mapping keys and YAML timestamps omitted for JSON compatibility)"
    )


if __name__ == "__main__":
    main()
