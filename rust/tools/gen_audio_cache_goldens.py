#!/usr/bin/env python3
"""Run the reference audio sniffer without importing the gateway runtime."""
import json
import ast
import runpy
import sys
from pathlib import Path

root = Path(__file__).resolve().parents[2]
sniff = runpy.run_path(str(root / 'tools/audio_container.py'))['sniff_audio_ext']
samples = [bytes([255, second]) for second in range(256)]
for signature in [b'OggS', b'fLaC', b'RIFF1234WAVE', b'RIFF1234WEBP',
                  b'ID3', b'\x1a\x45\xdf\xa3', b'1234ftypM4A ', b'1234ftypisom']:
    samples.extend(signature[:length] for length in range(len(signature) + 1))
rows = [dict(data=list(data), fallback=fallback, result=sniff(data, fallback))
        for data in samples for fallback in ['.ogg', 'mp3', '']]
path = root / 'rust/tools/audio-cache-goldens.json'
text = json.dumps(rows, indent=2) + '\n'
if sys.argv[1:] == ['--check']:
    assert path.read_text() == text
elif not sys.argv[1:]:
    path.write_text(text)
else:
    raise SystemExit('usage: gen_audio_cache_goldens.py [--check]')
print(f'Verified {len(rows)} audio cache extension cases')

# Extract the actual addressed-attachment audio branch, including its fallback
# allowlist. No Discord SDK import or handwritten copy of the selector is needed.
tree = ast.parse((root / 'plugins/platforms/discord/adapter.py').read_text())
branch = next(node for node in ast.walk(tree)
              if isinstance(node, ast.If)
              and ast.unparse(node.test) == "content_type.startswith('audio/')"
              and isinstance(node.body[0], ast.Try))
template = ast.parse('def select(content_type):\n    if not content_type.startswith("audio/"):\n        return None\n')
function = template.body[0]
function.body.extend(branch.body[0].body[:2])
function.body.append(ast.Return(value=ast.Name(id='ext', ctx=ast.Load())))
ns = {}
exec(compile(ast.fix_missing_locations(template), 'discord-audio-source', 'exec'), ns)
types = ['', 'audio/', 'audio/ogg', 'audio/mp3', 'audio/mpeg', 'audio/aac',
         'audio/wav', 'audio/webm', 'audio/m4a', 'audio/opus', 'audio/ogg; codecs=opus',
         'audio/OGG', 'Audio/ogg', 'video/ogg', 'application/ogg', 'audio/x/ogg']
rows = [dict(content_type=value, result=ns['select'](value)) for value in types]
path = root / 'rust/tools/discord-audio-goldens.json'
text = json.dumps(rows, indent=2) + '\n'
if sys.argv[1:] == ['--check']:
    assert path.read_text() == text
else:
    path.write_text(text)
print(f'Verified {len(rows)} Discord audio classification cases')

import os
import itertools
from typing import Any, Dict
tree = ast.parse((root / 'plugins/platforms/slack/adapter.py').read_text())
names = {'_resolve_slack_audio_ext', '_is_slack_voice_clip'}
constants = {'_SLACK_AUDIO_MIME_TO_EXT', '_SLACK_STT_SUPPORTED_EXTS'}
nodes = [node for node in tree.body
         if isinstance(node, ast.FunctionDef) and node.name in names
         or isinstance(node, ast.Assign) and any(isinstance(t, ast.Name) and t.id in constants for t in node.targets)]
ns = dict(os=os, Any=Any, Dict=Dict)
exec(compile(ast.Module(body=nodes, type_ignores=[]), 'slack-audio-source', 'exec'), ns)
rows = []
for mime, name, subtype in itertools.product(
        ['audio/ogg', 'audio/mp4', 'audio/x-wav', 'audio/aac', 'audio/flac', 'audio/mpeg; x=y', 'Audio/mp4', 'video/mp4', 'unknown'],
        ['', 'clip.MP3', 'audio_message123.mp4', ' clip.flac ', '.mp3', '...ogg', '/a.b/clip', 'clip.unsupported'],
        ['', 'slack_audio', 'slack_video']):
    file = dict(mimetype=mime, name=name, subtype=subtype)
    recognized = mime.startswith('audio/') or mime.startswith('video/') and ns['_is_slack_voice_clip'](file)
    rows.append(dict(file=file, result=ns['_resolve_slack_audio_ext'](file, mime) if recognized else None))
path = root / 'rust/tools/slack-audio-goldens.json'
text = json.dumps(rows, indent=2) + '\n'
if sys.argv[1:] == ['--check']:
    assert path.read_text() == text
else:
    path.write_text(text)
print(f'Verified {len(rows)} Slack audio classification cases')

cls = next(n for n in tree.body if isinstance(n, ast.ClassDef) and n.name == 'SlackAdapter')
helpers = [n for n in cls.body if isinstance(n, ast.FunctionDef) and n.name in {'_event_team_id', '_workspace_event_id'}]
for node in helpers:
    node.decorator_list = []
from typing import Optional
ns['Optional'] = Optional
exec(compile(ast.Module(body=helpers, type_ignores=[]), 'slack-workspace-source', 'exec'), ns)
rows = []
for inner, outer, auth in itertools.product(
        [{}, {'team':'inner'}, {'team_id':'primary','team':'secondary'}, {'team_id':{'id':'nested'}}, {'team_id':42}],
        [{}, {'team_id':'outer'}, {'team':{'id':'outer-nested'}}],
        [[], [{'team_id':'authorized'}]]):
    payload = dict(outer, event=dict(inner, ts='123.4'), authorizations=auth)
    team = ns['_event_team_id'](payload['event'], payload)
    rows.append(dict(payload=payload, result=ns['_workspace_event_id'](team, '123.4')))
path = root / 'rust/tools/slack-dedup-goldens.json'
text = json.dumps(rows, indent=2) + '\n'
if sys.argv[1:] == ['--check']:
    assert path.read_text() == text
else:
    path.write_text(text)
print(f'Verified {len(rows)} Slack workspace dedup cases')

# Execute the actual genuine-video suffix selection, without its network effects.
base_tree = ast.parse((root / 'gateway/platforms/base.py').read_text())
constants = [n for n in base_tree.body if isinstance(n, ast.Assign) and any(isinstance(t, ast.Name) and t.id == 'SUPPORTED_VIDEO_TYPES' for t in n.targets)]
exec(compile(ast.Module(body=constants, type_ignores=[]), 'video-types-source', 'exec'), ns)
branch = next(n for n in ast.walk(tree) if isinstance(n, ast.If) and ast.unparse(n.test) == "mimetype.startswith('video/') and url")
statements = branch.body[0].body[:4]
assert ast.unparse(statements[-1].test) == 'ext not in SUPPORTED_VIDEO_TYPES'
rows = []
for mime, name, subtype in itertools.product(
        ['video/mp4', 'video/quicktime', 'video/webm', 'video/x-matroska', 'video/x-msvideo', 'video/unknown', 'video/quicktime; codecs=x', 'Video/mp4', 'audio/mp4'],
        ['', 'clip.MOV', '.mov', '...webm', '/a.b/clip', 'clip.mkv', 'clip.unsupported', 'audio_message123.mp4'],
        ['', 'slack_audio']):
    file = dict(mimetype=mime, name=name, subtype=subtype)
    result = None
    if mime.startswith('video/') and not ns['_is_slack_voice_clip'](file):
        ns.update(f=file, mimetype=mime)
        exec(compile(ast.Module(body=statements, type_ignores=[]), 'slack-video-source', 'exec'), ns)
        result = ns['ext']
    rows.append(dict(file=file, result=result))
path = root / 'rust/tools/slack-video-goldens.json'
text = json.dumps(rows, indent=2) + '\n'
if sys.argv[1:] == ['--check']:
    assert path.read_text() == text
else:
    path.write_text(text)
print(f'Verified {len(rows)} Slack video classification cases')

# Run the actual bounded channel ownership cache with a small cap so the
# sequence exercises ambiguity and independent eviction of both dictionaries.
methods = [n for n in cls.body if isinstance(n, ast.FunctionDef) and n.name in {'_remember_channel_team', '_trim_oldest_dict_entries'}]
cache_class = ast.ClassDef(name='ChannelCacheOracle', bases=[], keywords=[], body=methods, decorator_list=[])
exec(compile(ast.fix_missing_locations(ast.Module(body=[cache_class], type_ignores=[])), 'slack-channel-cache-source', 'exec'), ns)
cache = ns['ChannelCacheOracle']()
cache._channel_team = {}
cache._channel_teams = {}
cache._CHANNEL_TEAM_MAX = 4
rows = []
for channel, team in [('', 'T1'), ('C1', ''), ('C1', 'T1'), ('C2', 'T2'), ('C1', 'T1'), ('C1', 'T2'), ('C1', 'T1'), ('C3', 'T1'), ('C4', 'T1'), ('C5', 'T1'), ('C1', 'T3'), ('C6', 'T2'), ('C7', 'T2'), ('C8', 'T2'), ('C1', 'T1')]:
    cache._remember_channel_team(channel, team)
    rows.append(dict(channel=channel, team=team, routes=list(cache._channel_team.items()), observed=[[c, sorted(t)] for c,t in cache._channel_teams.items()]))
path = root / 'rust/tools/slack-channel-goldens.json'
text = json.dumps(rows, indent=2) + '\n'
if sys.argv[1:] == ['--check']:
    assert path.read_text() == text
else:
    path.write_text(text)
print(f'Verified {len(rows)} Slack channel ownership transitions')

from types import SimpleNamespace
methods = [n for n in cls.body if isinstance(n, ast.FunctionDef) and n.name in {'_slack_ignored_channels', '_is_ignored_channel'}]
ignore_class = ast.ClassDef(name='IgnoredOracle', bases=[], keywords=[], body=methods, decorator_list=[])
exec(compile(ast.fix_missing_locations(ast.Module(body=[ignore_class], type_ignores=[])), 'slack-ignored-source', 'exec'), ns)
oracle = ns['IgnoredOracle']()
rows = []
for extra, legacy, channel in itertools.product(
        [None, [], ['C1', ' C2 '], ['C1,C2'], '*', ' C1, C2 ', True, 123],
        [None, '', '*', 'C3'], ['', 'C1', 'C1:123.4', 'C2', 'C3', 'C9']):
    oracle.config = SimpleNamespace(extra={'ignored_channels': extra})
    ns['os'] = SimpleNamespace(getenv=lambda key: legacy)
    rows.append(dict(extra=extra, legacy=legacy, channel=channel, result=oracle._is_ignored_channel(channel)))
path = root / 'rust/tools/slack-ignored-goldens.json'
text = json.dumps(rows, indent=2) + '\n'
if sys.argv[1:] == ['--check']:
    assert path.read_text() == text
else:
    path.write_text(text)
print(f'Verified {len(rows)} Slack ignored-channel cases')

methods = [n for n in cls.body if isinstance(n, ast.FunctionDef) and n.name in {'_slack_api_human_users', '_event_declares_bot_sender'}]
bot_class = ast.ClassDef(name='BotOracle', bases=[], keywords=[], body=methods, decorator_list=[])
helpers = [n for n in tree.body if isinstance(n, ast.FunctionDef) and n.name in {'_collect_slack_block_mentions', '_slack_mention_detection_text'}]
exec(compile(ast.fix_missing_locations(ast.Module(body=[bot_class, *helpers], type_ignores=[])), 'slack-bot-source', 'exec'), ns)
rows = []
for marker, allowed, blocks in itertools.product(
        [{}, {'bot_id':'B1'}, {'bot_id':None}, {'bot_profile':{'id':'B1'}}, {'subtype':'bot_message'}, {'user_profile':{'is_bot':True}}, {'app_id':'A1'}, {'app_id':'A1','client_msg_id':'human'}],
        [[], ['PEER']],
        [[], [{'type':'rich_text_section','elements':[{'type':'user','user_id':'BOT'}]}], [{'type':'rich_text_quote','elements':[{'type':'user','user_id':'BOT'}]}]]):
    oracle = ns['BotOracle']()
    oracle.config = SimpleNamespace(extra={'api_human_users':allowed})
    event = dict(user='PEER', text='caption', blocks=blocks, **marker)
    rows.append(dict(event=event, allowed=allowed, bot=oracle._event_declares_bot_sender(event), mentioned='<@BOT>' in ns['_slack_mention_detection_text'](event)))
path = root / 'rust/tools/slack-bot-goldens.json'
text = json.dumps(rows, indent=2) + '\n'
if sys.argv[1:] == ['--check']:
    assert path.read_text() == text
else:
    path.write_text(text)
print(f'Verified {len(rows)} Slack declared-bot and mention cases')

helper = next(n for n in tree.body if isinstance(n, ast.FunctionDef) and n.name == '_serialize_slack_blocks_for_agent')
ns['json'] = json
exec(compile(ast.Module(body=[helper], type_ignores=[]), 'slack-block-payload-source', 'exec'), ns)
rows = []
for blocks, limit in itertools.product([
    [], [{'type':'rich_text','elements':[{'type':'text','text':'authored'}]}],
    [{'type':'section','text':{'type':'mrkdwn','text':'hello 世界 👋'},'url':'https://private.invalid','secret':'omit'}],
    [{'type':'actions','elements':[{'type':'button','action_id':'go','text':{'type':'plain_text','text':'Go'},'value':'hidden','url':'https://private.invalid'}]}],
    [{'type':'section','fields':[{}, [], '', None, False, 0, {'text':'field'}], 'optional':False}],
    [{'type':'rich_text'}, {'type':'divider','unexpected':42}],
    [{'url':'hidden'}],
], [0, 17, 18, 40, 6000]):
    rows.append(dict(blocks=blocks, limit=limit, result=ns['_serialize_slack_blocks_for_agent'](blocks, limit)))
path = root / 'rust/tools/slack-block-payload-goldens.json'
text = json.dumps(rows, indent=2) + '\n'
if sys.argv[1:] == ['--check']:
    assert path.read_text() == text
else:
    path.write_text(text)
print(f'Verified {len(rows)} Slack Block Kit payload cases')

import re
ns['re'] = re
names = {'_SLACK_MRKDWN_LINK_RE','_SLACK_ENTITY_LABEL_RE','_SLACK_FENCED_CODE_RE','_SLACK_INLINE_CODE_RE','_SLACK_DATE_RE','_SLACK_PERMALINK_RE','_SLACK_INLINE_STYLE_RE','_SLACK_HTML_ENTITY_RE','_SLACK_HTML_ENTITIES'}
constants = [n for n in tree.body if isinstance(n, ast.Assign) and any(isinstance(t, ast.Name) and t.id in names for t in n.targets)]
helper_names = {'_slack_str_field','_slack_permalink_path','_render_slack_inline_element','_extract_text_from_slack_blocks','_unescape_slack_entities','_normalize_slack_text_for_dedupe','_extract_additional_text_from_slack_blocks'}
helpers = [n for n in tree.body if isinstance(n, ast.FunctionDef) and n.name in helper_names]
exec(compile(ast.Module(body=constants+helpers, type_ignores=[]), 'slack-rich-text-source', 'exec'), ns)
rows = []
elements = [
    {'type':'text','text':'hello'}, {'type':'link','url':'https://example.com','text':'Example'},
    {'type':'user','user_id':'BOT'}, {'type':'channel','channel_id':'C1'}, {'type':'emoji','name':'wave'},
    {'type':'broadcast'}, {'type':'date','fallback':'today'},
    {'type':'message_mention','channel_id':'C1','message_ts':'123.456'},
    {'type':'unknown','text':'label','url':'https://example.com'}, {'type':'color','value':'#00ff00'},
    {'type':'team','team_id':'T1'}, {'type':'usergroup','usergroup_id':'S1'},
]
for inline, kind, primary, bot in itertools.product(elements, ['rich_text_section','rich_text_quote','rich_text_preformatted','rich_text_list'], ['', 'hello', '*hello* <@BOT|name> <https://example.com|Example>', '```hello```'], ['', 'BOT']):
    section = {'type':'rich_text_section','elements':[inline]}
    element = section if kind == 'rich_text_section' else {'type':kind,'elements':[section],'style':'bullet'}
    blocks = [{'type':'rich_text','elements':[element]}]
    rows.append(dict(blocks=blocks, primary=primary, bot=bot, result=ns['_extract_additional_text_from_slack_blocks'](blocks, primary, bot)))
for inline, primary in [
    ({'type':'message_mention','channel_id':'C1','message_ts':'123.456'}, '<https://team.slack.com/archives/C1/p123456?thread_ts=123.456&amp;cid=C1>'),
    ({'type':'date','fallback':'today'}, '<!date^123^{date}|today>'),
    ({'type':'link','url':'https://example.com?a=1&b=2','text':'View'}, '<https://example.com?a=1&amp;b=2|View>'),
    ({'type':'text','text':'hello'}, '**_hello_**'),
    ({'type':'text','text':'hello'}, '`hello`'),
    ({'type':'text','text':'hello'}, '```hello world```'),
]:
    for kind in ['rich_text_section', 'rich_text_preformatted']:
        blocks = [{'type':'rich_text','elements':[{'type':kind,'elements':[inline]}]}]
        rows.append(dict(blocks=blocks, primary=primary, bot='', result=ns['_extract_additional_text_from_slack_blocks'](blocks, primary)))
path = root / 'rust/tools/slack-rich-text-goldens.json'
text = json.dumps(rows, indent=2) + '\n'
if sys.argv[1:] == ['--check']:
    assert path.read_text() == text
else:
    path.write_text(text)
print(f'Verified {len(rows)} Slack rich-text merge cases')

# Execute the live inbound branch, excluding its final diagnostic logger call.
import copy
branch = copy.deepcopy(next(n for n in ast.walk(cls) if isinstance(n, ast.If) and ast.unparse(n.test) == 'slack_attachments'))
append_if = branch.body[-1]
assert isinstance(append_if, ast.If) and ast.unparse(append_if.test) == 'att_parts'
assert isinstance(append_if.body[-1], ast.Expr) and ast.unparse(append_if.body[-1].value).startswith('logger.debug(')
append_if.body.pop()
code = compile(ast.Module(body=[branch], type_ignores=[]), 'slack-live-attachments-source', 'exec')
rows = []
for attachments, primary in itertools.product([
    [], [{'title':'Report','title_link':'https://example.com','text':'preview','footer':'Source'}],
    [{'from_url':'https://example.com','fallback':'fallback'}], [{'text':'body'}],
    [{'title':'only title'}], [{'title':'ignored','is_msg_unfurl':True}],
    [{'text':'世界👋'*200}], [{'text':'x'*500}], [{'text':'x'*501}],
    [{'text':'   ','fallback':'not selected'}], [{'footer':'alone'}],
    [{'title':'Report','text':'preview'}, {'title':'Report','text':'preview'}],
], ['', 'https://example.com', '📎 [Report](https://example.com)\n   preview', ' /help ']):
    ns.update(slack_attachments=attachments, text=primary)
    exec(code, ns)
    rows.append(dict(attachments=attachments, primary=primary, result=ns['text']))
path = root / 'rust/tools/slack-attachment-goldens.json'
text = json.dumps(rows, indent=2) + '\n'
if sys.argv[1:] == ['--check']:
    assert path.read_text() == text
else:
    path.write_text(text)
print(f'Verified {len(rows)} Slack live attachment cases')

methods = [n for n in cls.body if isinstance(n, ast.FunctionDef) and n.name in {'_slack_disable_dms', '_slack_allowed_channels'}]
policy_class = ast.ClassDef(name='ChannelPolicyOracle', bases=[], keywords=[], body=methods, decorator_list=[])
exec(compile(ast.fix_missing_locations(ast.Module(body=[policy_class], type_ignores=[])), 'slack-channel-policy-source', 'exec'), ns)
rows = []
for extra, legacy in itertools.product([None, False, True, 0, 2, [], ['C1', ' C2 '], '', ' on ', 'C1,C2'], [None, '', 'true', 'C3']):
    oracle = ns['ChannelPolicyOracle']()
    oracle.config = SimpleNamespace(extra={'disable_dms':extra, 'allowed_channels':extra})
    ns['os'] = SimpleNamespace(getenv=lambda key, default=None: legacy if legacy is not None else default)
    rows.append(dict(extra=extra, legacy=legacy, disabled=oracle._slack_disable_dms(), allowed=sorted(oracle._slack_allowed_channels())))
path = root / 'rust/tools/slack-channel-policy-goldens.json'
text = json.dumps(rows, indent=2) + '\n'
if sys.argv[1:] == ['--check']:
    assert path.read_text() == text
else:
    path.write_text(text)
print(f'Verified {len(rows)} Slack channel policy cases')

methods = [n for n in cls.body if isinstance(n, ast.FunctionDef) and n.name in {'_dm_top_level_threads_as_sessions', '_resolve_thread_ts'}]
thread_class = ast.ClassDef(name='ThreadOracle', bases=[], keywords=[], body=methods, decorator_list=[])
exec(compile(ast.fix_missing_locations(ast.Module(body=[thread_class], type_ignores=[])), 'slack-thread-source', 'exec'), ns)
branch = next(n for n in ast.walk(cls) if isinstance(n, ast.If) and ast.unparse(n.test) == 'is_dm' and '_dm_top_level_threads_as_sessions' in ast.unparse(n))
code = compile(ast.Module(body=[branch], type_ignores=[]), 'slack-thread-selection-source', 'exec')
rows = []
for kind, thread, reply, dm in itertools.product(['im','mpim','channel'], ['', '1', '0'], [True, False, None, 'false'], [None, True, False, 'off']):
    oracle = ns['ThreadOracle']()
    extra = {'reply_in_thread':reply, 'dm_top_level_threads_as_sessions':dm}
    oracle.config = SimpleNamespace(extra=extra)
    event = dict(channel='C1', channel_type=kind, ts='1', thread_ts=thread)
    ns.update(self=oracle, event=event, is_dm=kind in {'im','mpim'}, ts='1', assistant_meta={})
    exec(code, ns)
    selected = ns['thread_ts'] or None
    target = oracle._resolve_thread_ts('1', {'thread_id':selected}) or None
    rows.append(dict(extra=extra, event=event, selected=selected, target=target))
path = root / 'rust/tools/slack-thread-goldens.json'
text = json.dumps(rows, indent=2) + '\n'
if sys.argv[1:] == ['--check']:
    assert path.read_text() == text
else:
    path.write_text(text)
print(f'Verified {len(rows)} Slack thread routing cases')

# Execute the source's addressed-user and configuration helpers independently
# of the SDK. Include malformed tokens and Python-only whitespace boundaries.
methods = [n for n in cls.body if isinstance(n, ast.FunctionDef) and n.name in {
    '_slack_ignore_other_user_mentions', '_slack_message_addressed_to_other_user',
    '_slack_message_mentions_self'}]
mention_class = ast.ClassDef(name='MentionOracle', bases=[], keywords=[], body=methods, decorator_list=[])
import re
mention_ns = {'re': re}
helpers = [n for n in tree.body if isinstance(n, ast.FunctionDef) and n.name in {'_collect_slack_block_mentions', '_slack_mention_detection_text'}]
exec(compile(ast.fix_missing_locations(ast.Module(body=[mention_class, *helpers], type_ignores=[])), 'slack-addressed-source', 'exec'), mention_ns)
oracle = mention_ns['MentionOracle']()
rows = []
for text, blocks in itertools.product(
        ['', 'hello', '<@OTHER> hi', '\u001c<@OTHER|Name> hi', '<@BOT> hi',
         '<@BOTX> hi', '<@OTHER> <@BOT|Hermes> hi', 'ask <@OTHER>', '<!here> hi',
         '<@> hi', '<@OTHER name> hi', '<@OTHER|a>b> hi', '<@OTHER', '<@OTHER|> hi',
         '<@OTHER> <@PRIMARY> hi', '<@OTHER> <@BOTX> hi'],
        [[], [{'type':'rich_text_section','elements':[{'type':'user','user_id':'BOT'}]}],
         [{'type':'rich_text_quote','elements':[{'type':'user','user_id':'BOT'}]}]]):
    event = dict(text=text, blocks=blocks)
    routing = mention_ns['_slack_mention_detection_text'](event)
    rows.append(dict(event=event, routing=routing,
                     addressed=oracle._slack_message_addressed_to_other_user(routing, {'BOT', 'PRIMARY'}),
                     mentions_self=oracle._slack_message_mentions_self(routing, {'BOT', 'PRIMARY'})))
configs = []
for extra, legacy in itertools.product([None, False, True, 0, 2, [], [1], 'true', ' true ', 'OFF', 'yes'], [None, '', 'true', ' true ']):
    oracle.config = SimpleNamespace(extra={'ignore_other_user_mentions':extra})
    mention_ns['os'] = SimpleNamespace(getenv=lambda key, default='': legacy if legacy is not None else default)
    configs.append(dict(extra=extra, legacy=legacy, result=oracle._slack_ignore_other_user_mentions()))
path = root / 'rust/tools/slack-addressed-goldens.json'
text = json.dumps(dict(messages=rows, configs=configs), indent=2) + '\n'
if sys.argv[1:] == ['--check']:
    assert path.read_text() == text
else:
    path.write_text(text)
print(f'Verified {len(rows)} Slack addressed messages and {len(configs)} settings')

# Run the original strict/free/thread gate, stopping before the asynchronous
# wake checks. This measures only unconditional rejection, not permission to wake.
import copy
branch = copy.deepcopy(next(n for n in ast.walk(cls) if isinstance(n, ast.If)
                            and ast.unparse(n.test) == 'force_process'
                            and len(n.body) == 1 and isinstance(n.body[0], ast.Pass)))
current = branch
while current.orelse:
    following = current.orelse[0]
    if isinstance(following, ast.If) and ast.unparse(following.test) == 'not is_mentioned':
        current.orelse = []
        break
    current = following
for node in ast.walk(branch):
    if isinstance(node, ast.Return):
        node.value = ast.Constant(True)
module = ast.parse('def rejects(self, channel_id, force_process, is_thread_reply, is_mentioned, event_thread_ts):\n    pass\n')
module.body[0].body = [branch, ast.Return(ast.Constant(False))]
policy_ns = {'logger': SimpleNamespace(debug=lambda *args: None)}
exec(compile(ast.fix_missing_locations(module), 'slack-strict-source', 'exec'), policy_ns)
rows = []
for require, strict, thread, free, required, force, reply, mentioned in itertools.product([False, True], repeat=8):
    oracle = SimpleNamespace(
        _slack_require_mention=lambda:require, _slack_strict_mention=lambda:strict,
        _slack_thread_require_mention=lambda:thread,
        _slack_free_response_channels=lambda:{'C1'} if free else set(),
        _slack_require_mention_channels=lambda:{'C1'} if required else set())
    result = policy_ns['rejects'](oracle, 'C1', force, reply, mentioned, '1' if reply else '')
    rows.append(dict(require=require, strict=strict, thread=thread, free=free,
                     required=required, force=force, reply=reply, mentioned=mentioned, rejected=result))
path = root / 'rust/tools/slack-strict-goldens.json'
text = json.dumps(rows, indent=2) + '\n'
if sys.argv[1:] == ['--check']:
    assert path.read_text() == text
else:
    path.write_text(text)
print(f'Verified {len(rows)} Slack strict mention decisions')

# Tracking-set eviction uses timestamp order rather than insertion order.
methods = [copy.deepcopy(n) for n in cls.body if isinstance(n, ast.FunctionDef) and n.name in {
    '_slack_timestamp_sort_key', '_discard_oldest_slack_timestamps',
    '_trim_bot_message_timestamps', '_trim_mentioned_threads',
    '_workspace_message_marker', '_register_mentioned_thread'}]
marker_ns = {'Any': Any, 'Tuple': __import__('typing').Tuple}
marker_class = ast.ClassDef(name='MarkerOracle', bases=[], keywords=[], body=methods, decorator_list=[])
exec(compile(ast.fix_missing_locations(ast.Module(body=[marker_class], type_ignores=[])), 'slack-marker-source', 'exec'), marker_ns)
rows = []
for values in [
    ['9.2','1.000001','3.2','2.3','8.0','6.0','7.0','4.0','5.0'],
    ['bad','--1.0','-2.2','0.1','999999999999999999999999999999999.1','3.0'],
    ['1.1','1.000002','1.000001','1.12','1.2','1.0000009'],
]:
    oracle = marker_ns['MarkerOracle']()
    oracle._BOT_TS_MAX = oracle._MENTIONED_THREADS_MAX = 4
    oracle._bot_message_ts = set()
    oracle._mentioned_threads = set()
    for i, ts in enumerate(values):
        team = 'T1' if i % 2 else ''
        oracle._register_mentioned_thread(ts, team)
        oracle._bot_message_ts.add(oracle._workspace_message_marker(team, ts))
        oracle._trim_bot_message_timestamps()
        normalize = lambda markers: sorted([list(m) if isinstance(m, tuple) else ['', m] for m in markers])
        rows.append(dict(team=team, ts=ts, reset=i == 0, bot=normalize(oracle._bot_message_ts), mentioned=normalize(oracle._mentioned_threads)))
path = root / 'rust/tools/slack-thread-marker-goldens.json'
text = json.dumps(rows, indent=2) + '\n'
if sys.argv[1:] == ['--check']:
    assert path.read_text() == text
else:
    path.write_text(text)
print(f'Verified {len(rows)} Slack thread marker transitions')

helpers = [n for n in tree.body if isinstance(n, ast.FunctionDef) and n.name in {'_extract_text_from_slack_attachments', '_extract_urls_from_slack_blocks', '_slack_file_marker'}]
renderer = copy.deepcopy(next(n for n in cls.body if isinstance(n, ast.FunctionDef) and n.name == '_render_message_text'))
renderer.decorator_list = []
exec(compile(ast.fix_missing_locations(ast.Module(body=[*helpers, renderer], type_ignores=[])), 'slack-history-render-source', 'exec'), ns)
rows = []
for text, blocks, attachments, files, bot in itertools.product(
        ['', '<@BOT> hello', 'https://example.com?a=1&amp;b=2'],
        [[], [{'type':'section','text':{'type':'mrkdwn','text':'Alert'},'accessory':{'type':'button','url':'https://example.com?a=1&b=2'}}],
         [{'type':'rich_text','elements':[{'type':'rich_text_section','elements':[{'type':'text','text':'hello'},{'type':'user','user_id':'BOT'}]}]}],
         [{'type':'actions','elements':[{'url':'https://one','image_url':'https://two','external_url':'ftp://skip'},{'url':'https://one'}]}]],
        [[], [{'pretext':'Heads up','title':'Alert','text':'Body','fields':[{'title':'Host','value':'srv1'}],'fallback':'duplicate'}],
         [{'fallback':'fallback only'}], [{'is_msg_unfurl':True,'text':'unfurl'},{'blocks':[{'type':'rich_text','elements':[{'type':'rich_text_section','elements':[{'type':'text','text':'nested'}]}]}]}]],
        [[], [{'name':'chart.png','mimetype':'image/png'}], [{'name':'[evil]\nname','mimetype':'video/mp4'}],
         [{'name':'','title':'clip','mimetype':'audio/ogg'}, {'id':'F1','mimetype':'application/pdf'}],
         [None, {'name':'[]\r\n','mimetype':''}, {'name':12,'mimetype':123}]],
        ['', 'BOT']):
    message = dict(text=text, blocks=blocks, attachments=attachments, files=files)
    rows.append(dict(message=message, bot=bot, result=ns['_render_message_text'](message, bot)))
path = root / 'rust/tools/slack-history-text-goldens.json'
text = json.dumps(rows, indent=2) + '\n'
if sys.argv[1:] == ['--check']:
    assert path.read_text() == text
else:
    path.write_text(text)
print(f'Verified {len(rows)} Slack thread history renderings')

import asyncio
session_tree = ast.parse((root / 'gateway/session.py').read_text())
neutralizer = next(n for n in session_tree.body if isinstance(n, ast.FunctionDef) and n.name == 'neutralize_untrusted_inline_text')
constant = next(n for n in session_tree.body if isinstance(n, ast.Assign) and any(isinstance(t, ast.Name) and t.id == '_MAX_PROMPT_METADATA_CHARS' for t in n.targets))
ns['Tuple'] = __import__('typing').Tuple
ns['List'] = __import__('typing').List
exec(compile(ast.Module(body=[constant, neutralizer], type_ignores=[]), 'thread-neutralizer-source', 'exec'), ns)
formatter = copy.deepcopy(next(n for n in cls.body if isinstance(n, ast.AsyncFunctionDef) and n.name == '_format_thread_context'))
formatter.body = [n for n in formatter.body if not isinstance(n, ast.ImportFrom)]
exec(compile(ast.fix_missing_locations(ast.Module(body=[formatter], type_ignores=[])), 'slack-thread-format-source', 'exec'), ns)
rows = []
for after, current, authorized, bot_sender, team in itertools.product(['', '1', '2'], ['', '1', '3'], [None, False, True], [False, True], ['T1', 'T2']):
    messages = [dict(ts='1', user='HUMAN', text='<@BOT> root\n## injected'),
                dict(ts='2', user='BOT', text='prior\nreply', **({'bot_id':'B1'} if bot_sender else {})),
                dict(ts='3', user='OTHER', text='follow up')]
    names = {'HUMAN':'Name\n## heading', 'OTHER':'Other'}
    async def name(user_id, **kwargs):
        return names.get(user_id, user_id)
    bot_oracle = ns['BotOracle']()
    bot_oracle.config = SimpleNamespace(extra={'api_human_users':[]})
    oracle = SimpleNamespace(_team_bot_user_ids={'T1':'BOT', 'T2':'BOT2'}, _bot_user_id='PRIMARY',
        _event_declares_bot_sender=bot_oracle._event_declares_bot_sender,
        _render_message_text=ns['_render_message_text'], _is_sender_authorized=lambda *a, **kw:authorized,
        _resolve_user_name=name)
    content, parent = asyncio.run(ns['_format_thread_context'](oracle, messages, thread_ts='1', current_ts=current, team_id=team, channel_id='C1', after_ts=after))
    rows.append(dict(messages=messages, after=after, current=current, authorized=authorized, team=team,
                     names=names, content=content, parent=parent))
path = root / 'rust/tools/slack-thread-format-goldens.json'
text = json.dumps(rows, indent=2) + '\n'
if sys.argv[1:] == ['--check']:
    assert path.read_text() == text
else:
    path.write_text(text)
print(f'Verified {len(rows)} Slack thread formatting cases')

wake = copy.deepcopy(next(n for n in cls.body if isinstance(n, ast.AsyncFunctionDef) and n.name == '_should_wake_on_unmentioned_message'))
exec(compile(ast.Module(body=[wake], type_ignores=[]), 'slack-thread-wake-source', 'exec'), ns)
rows = []
for thread, sent, mentioned, active, authored, parent_mention in itertools.product(['', '2', '1'], [False, True], [False, True], [False, True], [False, True], [False, True]):
    calls, registered = [], []
    def active_probe(**kwargs):
        calls.append('active')
        return active
    async def authored_probe(**kwargs):
        calls.append('authored')
        return authored
    async def parent_probe(**kwargs):
        calls.append('parent')
        return '<@BOT> root' if parent_mention else 'root'
    oracle = SimpleNamespace(_workspace_message_marker=lambda team, ts:(team, ts) if team else ts,
        _bot_message_ts={('T1',thread)} if sent else set(), _mentioned_threads={('T1',thread)} if mentioned else set(),
        _has_active_session_for_thread=active_probe, _bot_authored_thread_root=authored_probe,
        _fetch_thread_parent_text=parent_probe, _team_bot_user_ids={'T1':'BOT'}, _bot_user_id='PRIMARY',
        _slack_strict_mention=lambda:False, _register_mentioned_thread=registered.append)
    result = asyncio.run(ns['_should_wake_on_unmentioned_message'](oracle, thread, 'C1', 'HUMAN', bool(thread and thread != '2'), team_id='T1'))
    rows.append(dict(thread=thread, sent=sent, mentioned=mentioned, active=active, authored=authored, parent_mention=parent_mention, result=result, active_called='active' in calls, registered=registered))
path = root / 'rust/tools/slack-thread-wake-goldens.json'
text = json.dumps(rows, indent=2) + '\n'
if sys.argv[1:] == ['--check']:
    assert path.read_text() == text
else:
    path.write_text(text)
print(f'Verified {len(rows)} Slack thread wake decisions')

from enum import Enum
key_ns = dict(Optional=__import__('typing').Optional, SessionSource=object,
              Platform=Enum('Platform', {'SLACK':'slack', 'WHATSAPP':'whatsapp'}))
key_helpers = [n for n in session_tree.body if isinstance(n, ast.FunctionDef) and n.name in {'build_session_key','_session_key_namespace'}]
exec(compile(ast.Module(body=key_helpers, type_ignores=[]), 'session-key-source', 'exec'), key_ns)
key_ns['SessionSource'] = lambda **kw:SimpleNamespace(user_id_alt=None, prospective_thread_id=None, **kw)
wrapper = copy.deepcopy(next(n for n in cls.body if isinstance(n, ast.FunctionDef) and n.name == '_build_thread_session_key'))
for node in ast.walk(wrapper):
    if isinstance(node, ast.Try):
        node.body = [n for n in node.body if not isinstance(n, ast.ImportFrom)]
exec(compile(ast.fix_missing_locations(ast.Module(body=[wrapper], type_ignores=[])), 'slack-session-key-source', 'exec'), key_ns)
rows = []
for kind, thread, team, user, group, per_thread, profile in itertools.product(['im','mpim','channel'], ['', '123.4'], ['', 'T1'], ['', 'U1'], [False, True], [False, True], [None, 'default', 'coder']):
    config = SimpleNamespace(group_sessions_per_user=group, thread_sessions_per_user=per_thread)
    oracle = SimpleNamespace(_session_store=SimpleNamespace(config=config), _session_key_profile=lambda source:profile)
    channel = {'im':'D1','mpim':'G1','channel':'C1'}[kind]
    result = key_ns['_build_thread_session_key'](oracle, channel, thread, user, team, chat_type='dm' if kind in {'im','mpim'} else 'group')
    rows.append(dict(event=dict(channel=channel, channel_type=kind, thread_ts=thread, user=user), team=team, group=group, per_thread=per_thread, profile=profile, result=result))
path = root / 'rust/tools/slack-thread-key-goldens.json'
text = json.dumps(rows, indent=2) + '\n'
if sys.argv[1:] == ['--check']:
    assert path.read_text() == text
else:
    path.write_text(text)
print(f'Verified {len(rows)} Slack thread session keys')
