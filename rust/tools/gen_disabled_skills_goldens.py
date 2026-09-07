#!/usr/bin/env python3
"""Generate goldens for get_disabled_skill_names and parse_config_string_list.

This script exercises the actual Python implementation in agent.skill_utils
under various combinations of config values, platform arguments, environment
variables (HERMES_PLATFORM), and session context (HERMES_SESSION_PLATFORM).

Literal parser edge cases that Rust must preserve:
1. Python single-quoted list literals (e.g. "['a', 'b']") are accepted by
   ast.literal_eval, whereas standard JSON requires double quotes.
2. Trailing commas inside the list (e.g. "['a', 'b',]") are valid in Python
   literals but invalid in standard JSON.
3. Case-sensitivity of booleans/nulls: in Python literals, True/False/None are
   valid and converted to strings ("True", "False", "None"). JSON-style lowercase
   literals (true, false, null) are NOT Python literal constants; ast.literal_eval
   rejects them with ValueError (ast.Name), causing fallback to the unparsed
   string as a single scalar entry.
4. Fallback on parse failure: when a string starts with '[' but fails literal_eval
   (malformed syntax, unclosed bracket, unquoted identifier, trailing characters,
   operator expressions), it falls back to returning the raw string as a single
   scalar name [value].
5. Implicit string concatenation: adjacent string literals without operators
   (e.g. '["foo" "bar"]') are concatenated by Python literal parsing into "foobar".
6. Non-string elements: elements inside parsed lists that are ints, floats, etc.
   are stringified via str(item).
7. Non-list AST results: any parsed expression that is not a Python list falls
   back to returning [value].
8. Leading/trailing whitespace: whitespace around bracketed strings is stripped
   before checking startswith('['). Individual elements are stripped and empty
   entries are filtered out by _normalize_string_set. Internal whitespace is
   preserved.
9. Essential skills immunity: "hermes-agent" is strictly case-sensitive.
   "hermes-agent" is removed from the disabled set, but "Hermes-Agent" or
   "HERMES-AGENT" is not immune and remains disabled.
10. Platform matching: platform names are matched against platform_disabled keys
    with exact case and whitespace; no trimming or lowercasing is applied to
    the platform key lookup.
11. Platform precedence: platform argument -> HERMES_PLATFORM env var ->
    HERMES_SESSION_PLATFORM session context var. Empty strings ("") are falsy
    and trigger fallback to the next source, whereas whitespace strings ("   ")
    are truthy and prevent fallback.
"""

import json
import os
from pathlib import Path
import sys
from unittest.mock import patch

REPO = Path(__file__).resolve().parents[2]
if str(REPO) not in sys.path:
    sys.path.insert(0, str(REPO))

import agent.skill_utils as skill_utils  # noqa: E402
import gateway.session_context as session_context  # noqa: E402

OUT = REPO / "rust/tools/disabled-skills-goldens.json"

RAW_CASES = [
    # -------------------------------------------------------------------------
    # Category 1: Global Disabled - Basic Types, Lists, and Scalar Normalization
    # -------------------------------------------------------------------------
    # 1. Empty config dictionary -> no disabled skills
    {
        "config": {},
        "platform": None,
        "env_platform": None,
        "session_platform": None,
    },
    # 2. Config with empty skills section
    {
        "config": {"skills": {}},
        "platform": None,
        "env_platform": None,
        "session_platform": None,
    },
    # 3. Config with empty disabled list
    {
        "config": {"skills": {"disabled": []}},
        "platform": None,
        "env_platform": None,
        "session_platform": None,
    },
    # 4. Single disabled skill in list
    {
        "config": {"skills": {"disabled": ["skill-a"]}},
        "platform": None,
        "env_platform": None,
        "session_platform": None,
    },
    # 5. Multiple disabled skills in list (alphabetical sorting verification)
    {
        "config": {"skills": {"disabled": ["skill-z", "skill-a", "skill-m"]}},
        "platform": None,
        "env_platform": None,
        "session_platform": None,
    },
    # 6. Duplicate skills in list (deduplication)
    {
        "config": {"skills": {"disabled": ["skill-a", "skill-a", "skill-b"]}},
        "platform": None,
        "env_platform": None,
        "session_platform": None,
    },
    # 7. Scalar string skill name
    {
        "config": {"skills": {"disabled": "skill-solo"}},
        "platform": None,
        "env_platform": None,
        "session_platform": None,
    },
    # 8. Scalar string with leading and trailing whitespace (stripped)
    {
        "config": {"skills": {"disabled": "  skill-trimmed  "}},
        "platform": None,
        "env_platform": None,
        "session_platform": None,
    },
    # 9. Empty scalar string -> filtered out
    {
        "config": {"skills": {"disabled": ""}},
        "platform": None,
        "env_platform": None,
        "session_platform": None,
    },
    # 10. Whitespace-only scalar string -> filtered out
    {
        "config": {"skills": {"disabled": "   \t\n  "}},
        "platform": None,
        "env_platform": None,
        "session_platform": None,
    },
    # 11. Scalar string with internal whitespace (internal whitespace preserved)
    {
        "config": {"skills": {"disabled": "skill with internal spaces"}},
        "platform": None,
        "env_platform": None,
        "session_platform": None,
    },
    # 12. Scalar string looking like JSON object (not starting with '[') -> scalar name
    {
        "config": {"skills": {"disabled": '{"skill": 1}'}},
        "platform": None,
        "env_platform": None,
        "session_platform": None,
    },
    # 13. Scalar string looking like tuple (not starting with '[') -> scalar name
    {
        "config": {"skills": {"disabled": "('skill-a', 'skill-b')"}},
        "platform": None,
        "env_platform": None,
        "session_platform": None,
    },
    # 14. Scalar string with non-ASCII whitespace (U+00A0 non-breaking space stripped)
    {
        "config": {"skills": {"disabled": "\u00a0skill-nobrk\u00a0"}},
        "platform": None,
        "env_platform": None,
        "session_platform": None,
    },

    # -------------------------------------------------------------------------
    # Category 2: Essential Skill Immunity (`hermes-agent`) & Case Sensitivity
    # -------------------------------------------------------------------------
    # 15. Global disabled has exact 'hermes-agent' scalar -> immune, stripped
    {
        "config": {"skills": {"disabled": "hermes-agent"}},
        "platform": None,
        "env_platform": None,
        "session_platform": None,
    },
    # 16. Global disabled has 'hermes-agent' in a list -> immune, stripped
    {
        "config": {"skills": {"disabled": ["hermes-agent"]}},
        "platform": None,
        "env_platform": None,
        "session_platform": None,
    },
    # 17. Global disabled has 'hermes-agent' mixed with other skills -> other skills kept
    {
        "config": {"skills": {"disabled": ["hermes-agent", "custom-skill", "another-skill"]}},
        "platform": None,
        "env_platform": None,
        "session_platform": None,
    },
    # 18. Global disabled has whitespace-padded '  hermes-agent  ' -> stripped then matched as immune
    {
        "config": {"skills": {"disabled": "  hermes-agent  "}},
        "platform": None,
        "env_platform": None,
        "session_platform": None,
    },
    # 19. Case sensitivity: 'Hermes-Agent' capitalized is NOT immune (exact match required)
    {
        "config": {"skills": {"disabled": "Hermes-Agent"}},
        "platform": None,
        "env_platform": None,
        "session_platform": None,
    },
    # 20. Case sensitivity: 'HERMES-AGENT' uppercase is NOT immune
    {
        "config": {"skills": {"disabled": "HERMES-AGENT"}},
        "platform": None,
        "env_platform": None,
        "session_platform": None,
    },
    # 21. Case sensitivity: 'hermes_agent' with underscore is NOT immune
    {
        "config": {"skills": {"disabled": "hermes_agent"}},
        "platform": None,
        "env_platform": None,
        "session_platform": None,
    },
    # 22. Substring match: 'hermes-agent-extra' is NOT immune
    {
        "config": {"skills": {"disabled": "hermes-agent-extra"}},
        "platform": None,
        "env_platform": None,
        "session_platform": None,
    },
    # 23. Platform-disabled has 'hermes-agent' -> immune, stripped
    {
        "config": {"skills": {"platform_disabled": {"cli": "hermes-agent"}}},
        "platform": "cli",
        "env_platform": None,
        "session_platform": None,
    },
    # 24. Platform-disabled has 'HERMES-AGENT' -> NOT immune, preserved
    {
        "config": {"skills": {"platform_disabled": {"cli": "HERMES-AGENT"}}},
        "platform": "cli",
        "env_platform": None,
        "session_platform": None,
    },
    # 25. Both global and platform specify hermes-agent alongside allowed skills
    {
        "config": {
            "skills": {
                "disabled": ["hermes-agent", "global-kept"],
                "platform_disabled": {"cli": ["hermes-agent", "platform-kept"]},
            }
        },
        "platform": "cli",
        "env_platform": None,
        "session_platform": None,
    },

    # -------------------------------------------------------------------------
    # Category 3: JSON-Array Strings & Python Literal-Array Strings
    # -------------------------------------------------------------------------
    # 26. Standard JSON-array string with double quotes
    {
        "config": {"skills": {"disabled": '["skill-1", "skill-2"]'}},
        "platform": None,
        "env_platform": None,
        "session_platform": None,
    },
    # 27. Python literal-array string with single quotes
    {
        "config": {"skills": {"disabled": "['skill-1', 'skill-2']"}},
        "platform": None,
        "env_platform": None,
        "session_platform": None,
    },
    # 28. Python literal-array string with mixed quotes
    {
        "config": {"skills": {"disabled": "['skill-1', \"skill-2\"]"}},
        "platform": None,
        "env_platform": None,
        "session_platform": None,
    },
    # 29. Python literal-array string with trailing comma
    {
        "config": {"skills": {"disabled": "['skill-1', 'skill-2',]"}},
        "platform": None,
        "env_platform": None,
        "session_platform": None,
    },
    # 30. Empty array string '[]'
    {
        "config": {"skills": {"disabled": "[]"}},
        "platform": None,
        "env_platform": None,
        "session_platform": None,
    },
    # 31. Empty array string with whitespace '[   ]'
    {
        "config": {"skills": {"disabled": "[   ]"}},
        "platform": None,
        "env_platform": None,
        "session_platform": None,
    },
    # 32. Array string with whitespace padded items
    {
        "config": {"skills": {"disabled": "['  skill-1  ', '  skill-2  ']"}},
        "platform": None,
        "env_platform": None,
        "session_platform": None,
    },
    # 33. Array string with empty and whitespace-only elements
    {
        "config": {"skills": {"disabled": "['skill-1', '', '   ', 'skill-2']"}},
        "platform": None,
        "env_platform": None,
        "session_platform": None,
    },
    # 34. Array string containing 'hermes-agent' (filtered out)
    {
        "config": {"skills": {"disabled": "['hermes-agent', 'skill-1']"}},
        "platform": None,
        "env_platform": None,
        "session_platform": None,
    },
    # 35. Array string containing 'Hermes-Agent' (preserved)
    {
        "config": {"skills": {"disabled": "['Hermes-Agent', 'skill-1']"}},
        "platform": None,
        "env_platform": None,
        "session_platform": None,
    },
    # 36. Python literal implicit string concatenation: adjacent string literals merged
    {
        "config": {"skills": {"disabled": '["skill" "-concat"]'}},
        "platform": None,
        "env_platform": None,
        "session_platform": None,
    },
    # 37. Python literal with comment inside array string
    {
        "config": {"skills": {"disabled": "['skill-a', # comment\n'skill-b']"}},
        "platform": None,
        "env_platform": None,
        "session_platform": None,
    },
    # 38. Python literal with hex escape sequence
    {
        "config": {"skills": {"disabled": "['skill-\\x61']"}},
        "platform": None,
        "env_platform": None,
        "session_platform": None,
    },
    # 39. Array string with Python boolean and None literals (converted to str)
    {
        "config": {"skills": {"disabled": "[True, False, None]"}},
        "platform": None,
        "env_platform": None,
        "session_platform": None,
    },
    # 40. Array string with JSON booleans/null (ast.literal_eval rejects -> fallback to scalar)
    {
        "config": {"skills": {"disabled": "[true, false, null]"}},
        "platform": None,
        "env_platform": None,
        "session_platform": None,
    },
    # 41. Array string with integer numbers (converted to str)
    {
        "config": {"skills": {"disabled": "[10, 20]"}},
        "platform": None,
        "env_platform": None,
        "session_platform": None,
    },
    # 42. Malformed array string: unclosed bracket (falls back to entire string as scalar)
    {
        "config": {"skills": {"disabled": '["skill-a", "skill-b"'}},
        "platform": None,
        "env_platform": None,
        "session_platform": None,
    },
    # 43. Malformed array string: unquoted bare identifier (falls back to scalar)
    {
        "config": {"skills": {"disabled": "[skill-a]"}},
        "platform": None,
        "env_platform": None,
        "session_platform": None,
    },
    # 44. Array string with trailing characters after closing bracket (falls back to scalar)
    {
        "config": {"skills": {"disabled": '["skill-a"] trailing'}},
        "platform": None,
        "env_platform": None,
        "session_platform": None,
    },
    # 45. Array string with binary addition operator (ast.literal_eval rejects -> falls back to scalar)
    {
        "config": {"skills": {"disabled": '["foo" + "bar"]'}},
        "platform": None,
        "env_platform": None,
        "session_platform": None,
    },
    # 46. Array string with raw unescaped newline in single-quoted string (syntax error -> scalar)
    {
        "config": {"skills": {"disabled": '["skill-1\nskill-2"]'}},
        "platform": None,
        "env_platform": None,
        "session_platform": None,
    },
    # 47. Triple-quoted string inside literal array
    {
        "config": {"skills": {"disabled": "['''skill-triple''']"}},
        "platform": None,
        "env_platform": None,
        "session_platform": None,
    },
    # 48. Array string with nested tuple inside list (converted to str)
    {
        "config": {"skills": {"disabled": "[(1, 2), 'skill-a']"}},
        "platform": None,
        "env_platform": None,
        "session_platform": None,
    },
    # 49. Multiline literal array string with newlines and indentation
    {
        "config": {"skills": {"disabled": "[\n  'line-a',\n  'line-b'\n]"}},
        "platform": None,
        "env_platform": None,
        "session_platform": None,
    },
    # 50. Array string with comment only -> empty array
    {
        "config": {"skills": {"disabled": "[\n# just a comment\n]"}},
        "platform": None,
        "env_platform": None,
        "session_platform": None,
    },
    # 51. Array string with escaped quote inside string literal
    {
        "config": {"skills": {"disabled": '["skill\\"escaped"]'}},
        "platform": None,
        "env_platform": None,
        "session_platform": None,
    },

    # -------------------------------------------------------------------------
    # Category 4: Global and Platform Union
    # -------------------------------------------------------------------------
    # 52. Disjoint global and platform disabled lists
    {
        "config": {
            "skills": {
                "disabled": ["g1", "g2"],
                "platform_disabled": {"cli": ["p1", "p2"]},
            }
        },
        "platform": "cli",
        "env_platform": None,
        "session_platform": None,
    },
    # 53. Overlapping global and platform disabled lists (deduplicated)
    {
        "config": {
            "skills": {
                "disabled": ["shared", "g1"],
                "platform_disabled": {"cli": ["shared", "p1"]},
            }
        },
        "platform": "cli",
        "env_platform": None,
        "session_platform": None,
    },
    # 54. Global disabled present, platform_disabled empty dict
    {
        "config": {
            "skills": {
                "disabled": ["g1"],
                "platform_disabled": {},
            }
        },
        "platform": "cli",
        "env_platform": None,
        "session_platform": None,
    },
    # 55. Global disabled empty, platform disabled populated
    {
        "config": {
            "skills": {
                "disabled": [],
                "platform_disabled": {"cli": ["p1", "p2"]},
            }
        },
        "platform": "cli",
        "env_platform": None,
        "session_platform": None,
    },
    # 56. Multiple platforms configured, query first platform
    {
        "config": {
            "skills": {
                "platform_disabled": {
                    "telegram": ["tele-skill"],
                    "discord": ["disc-skill"],
                }
            }
        },
        "platform": "telegram",
        "env_platform": None,
        "session_platform": None,
    },
    # 57. Multiple platforms configured, query second platform
    {
        "config": {
            "skills": {
                "platform_disabled": {
                    "telegram": ["tele-skill"],
                    "discord": ["disc-skill"],
                }
            }
        },
        "platform": "discord",
        "env_platform": None,
        "session_platform": None,
    },
    # 58. Platform disabled is scalar string
    {
        "config": {
            "skills": {
                "platform_disabled": {"cli": "scalar-platform"}
            }
        },
        "platform": "cli",
        "env_platform": None,
        "session_platform": None,
    },
    # 59. Platform disabled is JSON-array string
    {
        "config": {
            "skills": {
                "platform_disabled": {"cli": '["json-p1", "json-p2"]'}
            }
        },
        "platform": "cli",
        "env_platform": None,
        "session_platform": None,
    },
    # 60. Platform disabled is Python literal-array string
    {
        "config": {
            "skills": {
                "platform_disabled": {"cli": "['py-p1', 'py-p2']"}
            }
        },
        "platform": "cli",
        "env_platform": None,
        "session_platform": None,
    },
    # 61. Platform disabled key exists with value null -> returns only global
    {
        "config": {
            "skills": {
                "disabled": ["g1"],
                "platform_disabled": {"cli": None},
            }
        },
        "platform": "cli",
        "env_platform": None,
        "session_platform": None,
    },
    # 62. Platform disabled key exists with empty list -> returns only global
    {
        "config": {
            "skills": {
                "disabled": ["g1"],
                "platform_disabled": {"cli": []},
            }
        },
        "platform": "cli",
        "env_platform": None,
        "session_platform": None,
    },
    # 63. Queried platform is not present in platform_disabled -> returns only global
    {
        "config": {
            "skills": {
                "disabled": ["g1"],
                "platform_disabled": {"telegram": ["t1"]},
            }
        },
        "platform": "cli",
        "env_platform": None,
        "session_platform": None,
    },

    # -------------------------------------------------------------------------
    # Category 5: Exact Platform Matching (Casing & Whitespace)
    # -------------------------------------------------------------------------
    # 64. Casing mismatch: config has lowercase 'telegram', query 'Telegram' -> no match
    {
        "config": {
            "skills": {
                "platform_disabled": {"telegram": ["tele-skill"]}
            }
        },
        "platform": "Telegram",
        "env_platform": None,
        "session_platform": None,
    },
    # 65. Casing mismatch: config has 'Telegram', query 'telegram' -> no match
    {
        "config": {
            "skills": {
                "platform_disabled": {"Telegram": ["tele-skill"]}
            }
        },
        "platform": "telegram",
        "env_platform": None,
        "session_platform": None,
    },
    # 66. Exact uppercase match: config 'TELEGRAM', query 'TELEGRAM' -> matches
    {
        "config": {
            "skills": {
                "platform_disabled": {"TELEGRAM": ["tele-skill"]}
            }
        },
        "platform": "TELEGRAM",
        "env_platform": None,
        "session_platform": None,
    },
    # 67. Exact mixed-case match: config 'DiscordBot', query 'DiscordBot' -> matches
    {
        "config": {
            "skills": {
                "platform_disabled": {"DiscordBot": ["disc-skill"]}
            }
        },
        "platform": "DiscordBot",
        "env_platform": None,
        "session_platform": None,
    },
    # 68. Whitespace in platform argument: query ' telegram ', config 'telegram' -> no match
    {
        "config": {
            "skills": {
                "platform_disabled": {"telegram": ["tele-skill"]}
            }
        },
        "platform": " telegram ",
        "env_platform": None,
        "session_platform": None,
    },
    # 69. Whitespace in config key: config ' telegram ', query ' telegram ' -> exact match
    {
        "config": {
            "skills": {
                "platform_disabled": {" telegram ": ["tele-skill"]}
            }
        },
        "platform": " telegram ",
        "env_platform": None,
        "session_platform": None,
    },
    # 70. Whitespace-only platform matches exact whitespace key
    {
        "config": {
            "skills": {
                "platform_disabled": {"   ": ["space-skill"]}
            }
        },
        "platform": "   ",
        "env_platform": None,
        "session_platform": None,
    },
    # 71. Whitespace-only platform does not match missing key
    {
        "config": {
            "skills": {
                "platform_disabled": {"cli": ["cli-skill"]}
            }
        },
        "platform": "   ",
        "env_platform": None,
        "session_platform": None,
    },

    # -------------------------------------------------------------------------
    # Category 6: Platform Resolution Precedence (Arg vs Env vs Session)
    # -------------------------------------------------------------------------
    # 72. Arg provided: takes precedence over env and session
    {
        "config": {
            "skills": {
                "platform_disabled": {
                    "arg_p": ["p_arg"],
                    "env_p": ["p_env"],
                    "sess_p": ["p_sess"],
                }
            }
        },
        "platform": "arg_p",
        "env_platform": "env_p",
        "session_platform": "sess_p",
    },
    # 73. Arg is None: env_platform takes precedence over session_platform
    {
        "config": {
            "skills": {
                "platform_disabled": {
                    "env_p": ["p_env"],
                    "sess_p": ["p_sess"],
                }
            }
        },
        "platform": None,
        "env_platform": "env_p",
        "session_platform": "sess_p",
    },
    # 74. Arg is empty string "": falsy, falls back to env_platform
    {
        "config": {
            "skills": {
                "platform_disabled": {
                    "env_p": ["p_env"],
                    "sess_p": ["p_sess"],
                }
            }
        },
        "platform": "",
        "env_platform": "env_p",
        "session_platform": "sess_p",
    },
    # 75. Arg is None, env is None: falls back to session_platform
    {
        "config": {
            "skills": {
                "platform_disabled": {
                    "sess_p": ["p_sess"],
                }
            }
        },
        "platform": None,
        "env_platform": None,
        "session_platform": "sess_p",
    },
    # 76. Arg is None, env is empty string "": falls back to session_platform
    {
        "config": {
            "skills": {
                "platform_disabled": {
                    "sess_p": ["p_sess"],
                }
            }
        },
        "platform": None,
        "env_platform": "",
        "session_platform": "sess_p",
    },
    # 77. Arg empty string "", env empty string "": falls back to session_platform
    {
        "config": {
            "skills": {
                "platform_disabled": {
                    "sess_p": ["p_sess"],
                }
            }
        },
        "platform": "",
        "env_platform": "",
        "session_platform": "sess_p",
    },
    # 78. All three are None: no platform resolved -> only global disabled
    {
        "config": {
            "skills": {
                "disabled": ["g1"],
                "platform_disabled": {"cli": ["p1"]},
            }
        },
        "platform": None,
        "env_platform": None,
        "session_platform": None,
    },
    # 79. All three are empty strings "": no platform resolved -> only global disabled
    {
        "config": {
            "skills": {
                "disabled": ["g1"],
                "platform_disabled": {"": ["empty-skill"], "cli": ["p1"]},
            }
        },
        "platform": "",
        "env_platform": "",
        "session_platform": "",
    },
    # 80. Arg is None, env has whitespace ' telegram ' -> resolves to ' telegram '
    {
        "config": {
            "skills": {
                "platform_disabled": {
                    " telegram ": ["t_space"],
                    "discord": ["d"],
                }
            }
        },
        "platform": None,
        "env_platform": " telegram ",
        "session_platform": "discord",
    },
    # 81. Arg is None, env is None, session has whitespace ' discord ' -> resolves to ' discord '
    {
        "config": {
            "skills": {
                "platform_disabled": {
                    " discord ": ["d_space"],
                }
            }
        },
        "platform": None,
        "env_platform": None,
        "session_platform": " discord ",
    },

    # -------------------------------------------------------------------------
    # Category 7: Malformed Config Values, Types, & Edge Error States
    # -------------------------------------------------------------------------
    # 82. Root config is None (falsy) -> empty set
    {
        "config": None,
        "platform": None,
        "env_platform": None,
        "session_platform": None,
    },
    # 83. Root config is boolean False (falsy) -> empty set
    {
        "config": False,
        "platform": None,
        "env_platform": None,
        "session_platform": None,
    },
    # 84. Root config is empty list [] (falsy) -> empty set
    {
        "config": [],
        "platform": None,
        "env_platform": None,
        "session_platform": None,
    },
    # 85. Root config is empty string "" (falsy) -> empty set
    {
        "config": "",
        "platform": None,
        "env_platform": None,
        "session_platform": None,
    },
    # 86. Root config is boolean True (truthy non-dict) -> AttributeError "error"
    {
        "config": True,
        "platform": None,
        "env_platform": None,
        "session_platform": None,
    },
    # 87. Root config is integer 123 (truthy non-dict) -> AttributeError "error"
    {
        "config": 123,
        "platform": None,
        "env_platform": None,
        "session_platform": None,
    },
    # 88. Root config is non-empty string (truthy non-dict) -> AttributeError "error"
    {
        "config": "bad_config",
        "platform": None,
        "env_platform": None,
        "session_platform": None,
    },
    # 89. Root config is non-empty list (truthy non-dict) -> AttributeError "error"
    {
        "config": ["item1", "item2"],
        "platform": None,
        "env_platform": None,
        "session_platform": None,
    },
    # 90. skills section is None (not a dict) -> empty set
    {
        "config": {"skills": None},
        "platform": None,
        "env_platform": None,
        "session_platform": None,
    },
    # 91. skills section is integer (not a dict) -> empty set
    {
        "config": {"skills": 42},
        "platform": None,
        "env_platform": None,
        "session_platform": None,
    },
    # 92. skills section is string (not a dict) -> empty set
    {
        "config": {"skills": "invalid_skills"},
        "platform": None,
        "env_platform": None,
        "session_platform": None,
    },
    # 93. skills section is list (not a dict) -> empty set
    {
        "config": {"skills": ["bad_list"]},
        "platform": None,
        "env_platform": None,
        "session_platform": None,
    },
    # 94. disabled setting is None -> empty list
    {
        "config": {"skills": {"disabled": None}},
        "platform": None,
        "env_platform": None,
        "session_platform": None,
    },
    # 95. disabled setting is integer -> not iterable or str -> empty list
    {
        "config": {"skills": {"disabled": 123}},
        "platform": None,
        "env_platform": None,
        "session_platform": None,
    },
    # 96. disabled setting is boolean True -> not iterable or str -> empty list
    {
        "config": {"skills": {"disabled": True}},
        "platform": None,
        "env_platform": None,
        "session_platform": None,
    },
    # 97. disabled setting is dict -> not in (list, tuple, set, frozenset) -> empty list
    {
        "config": {"skills": {"disabled": {"nested": "dict"}}},
        "platform": None,
        "env_platform": None,
        "session_platform": None,
    },
    # 98. disabled setting is list of integers -> stringified ["1", "2", "3"]
    {
        "config": {"skills": {"disabled": [1, 2, 3]}},
        "platform": None,
        "env_platform": None,
        "session_platform": None,
    },
    # 99. disabled setting is list of mixed types (None, bool, str) -> stringified
    {
        "config": {"skills": {"disabled": [None, True, False, "custom-skill"]}},
        "platform": None,
        "env_platform": None,
        "session_platform": None,
    },
    # 100. disabled setting is list of dict objects -> stringified
    {
        "config": {"skills": {"disabled": [{"key": "val"}]}},
        "platform": None,
        "env_platform": None,
        "session_platform": None,
    },
    # 101. platform_disabled is None with platform resolved -> behaves like empty dict
    {
        "config": {
            "skills": {"disabled": ["g1"], "platform_disabled": None}
        },
        "platform": "cli",
        "env_platform": None,
        "session_platform": None,
    },
    # 102. platform_disabled is string with platform resolved -> AttributeError "error"
    {
        "config": {
            "skills": {"disabled": ["g1"], "platform_disabled": "not_a_dict"}
        },
        "platform": "cli",
        "env_platform": None,
        "session_platform": None,
    },
    # 103. platform_disabled is integer with platform resolved -> AttributeError "error"
    {
        "config": {
            "skills": {"disabled": ["g1"], "platform_disabled": 999}
        },
        "platform": "cli",
        "env_platform": None,
        "session_platform": None,
    },
    # 104. platform_disabled is list with platform resolved -> AttributeError "error"
    {
        "config": {
            "skills": {"disabled": ["g1"], "platform_disabled": ["bad_list"]}
        },
        "platform": "cli",
        "env_platform": None,
        "session_platform": None,
    },
    # 105. platform_disabled is string with platform NOT resolved -> no error, platform_disabled ignored
    {
        "config": {
            "skills": {"disabled": ["g1"], "platform_disabled": "not_a_dict"}
        },
        "platform": None,
        "env_platform": None,
        "session_platform": None,
    },
    # 106. platform_disabled value for platform is integer -> empty list
    {
        "config": {
            "skills": {"platform_disabled": {"cli": 456}}
        },
        "platform": "cli",
        "env_platform": None,
        "session_platform": None,
    },
    # 107. platform_disabled value for platform is dict -> empty list
    {
        "config": {
            "skills": {"platform_disabled": {"cli": {"nested": "dict"}}}
        },
        "platform": "cli",
        "env_platform": None,
        "session_platform": None,
    },
    # 108. platform_disabled value for platform is list with hermes-agent -> filtered
    {
        "config": {
            "skills": {"platform_disabled": {"cli": [100, "skill-p", "hermes-agent"]}}
        },
        "platform": "cli",
        "env_platform": None,
        "session_platform": None,
    },
    # 109. platform parameter is unhashable list -> TypeError "error"
    {
        "config": {
            "skills": {"platform_disabled": {"cli": ["p1"]}}
        },
        "platform": ["cli"],
        "env_platform": None,
        "session_platform": None,
    },
    # 110. platform parameter is unhashable dict -> TypeError "error"
    {
        "config": {
            "skills": {"platform_disabled": {"cli": ["p1"]}}
        },
        "platform": {"name": "cli"},
        "env_platform": None,
        "session_platform": None,
    },
]


def evaluate_case(config, platform, env_platform, session_platform):
    """Run get_disabled_skill_names with patched configuration and environment."""
    env_patch = {}
    if env_platform is not None:
        env_patch["HERMES_PLATFORM"] = str(env_platform)

    with patch.dict(os.environ, env_patch, clear=True):
        with patch.object(skill_utils, "_load_raw_config", return_value=config):
            with patch.object(session_context, "get_session_env", return_value=session_platform):
                try:
                    res = skill_utils.get_disabled_skill_names(platform)
                    return sorted(res)
                except Exception:
                    return "error"


def generate():
    """Generate all test cases with their expected outputs."""
    cases = []
    for item in RAW_CASES:
        config = item["config"]
        platform = item["platform"]
        env_platform = item["env_platform"]
        session_platform = item["session_platform"]
        expected = evaluate_case(config, platform, env_platform, session_platform)
        cases.append({
            "config": config,
            "platform": platform,
            "env_platform": env_platform,
            "session_platform": session_platform,
            "expected": expected,
        })
    return json.dumps(cases, indent=2) + "\n"


if __name__ == "__main__":
    content = generate()
    if sys.argv[1:] == ["--check"]:
        if not OUT.exists() or OUT.read_text(encoding="utf-8") != content:
            raise SystemExit("Disabled skills fixtures differ from Python")
    elif sys.argv[1:]:
        raise SystemExit("usage: gen_disabled_skills_goldens.py [--check]")
    else:
        OUT.write_text(content, encoding="utf-8")
    print(f"Verified {len(json.loads(content))} Python disabled skills cases")
