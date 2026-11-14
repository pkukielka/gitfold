#!/usr/bin/env python3
# How many deliberate reads (Read tool calls, and files simple viewers show
# in Bash commands) Claude Code transcripts hold, and how many of those a
# read-tracking hook that ignored Bash would miss: viewer reads of a file an
# earlier search in the session had read, so the kernel would serve it from
# cache. Reads ~/.claude/projects/*/*.jsonl and subagent transcripts; prints
# aggregates only. See ARCHITECTURE.md, "Overlaps".
# usage: python3 bench/transcript-reads.py [session-id-prefix] [-v]
import json, os, re, shlex, sys, glob
from collections import defaultdict, Counter

BASE = os.path.expanduser('~/.claude/projects')
VIEWERS = {'cat','head','tail','less','more','nl','tac','bat','sed'}
SEARCHERS = {'rg','grep','egrep','fgrep','ugrep','ag','ack'}
SEPS = {'|','||','&&',';','&','\n','|&',';;','(',')'}
HEAD_VAL = {'-n','-c','--lines','--bytes'}
SED_VAL = {'-e','-f','--expression','--file','-l'}
BAT_VAL = {'-r','--line-range','-l','--language','-H','--highlight-line','--style','--theme','-m','--map-syntax','--tabs','--wrap','--color','--paging','--terminal-width'}
NL_VAL = {'-b','-d','-f','-h','-i','-l','-n','-s','-v','-w'}
RG_VAL = {'-e','-f','-g','--glob','-t','--type','-T','--type-not','-A','-B','-C','-m','--max-count','-M','--max-columns','-d','--max-depth','--iglob','-E','--encoding','-r','--replace','--type-add','-j','--threads','--sort','--sortr','--context','--after-context','--before-context','--color','--colors','--max-filesize','--pre','--pre-glob','--path-separator','--ignore-file'}
GREP_VAL = {'-e','-f','-A','-B','-C','-m','--include','--exclude','--exclude-dir','--regexp','--file','--max-count','--context','--color','--label','-d','-D','--binary-files'}
HEREDOC = re.compile(r"<<-?\s*(['\"]?)([A-Za-z_][A-Za-z0-9_]*)\1")

def strip_heredocs(cmd):
    out, lines, i = [], cmd.split('\n'), 0
    while i < len(lines):
        l = lines[i]; out.append(l)
        delims = [m.group(2) for m in HEREDOC.finditer(l)]
        i += 1
        for d in delims:
            while i < len(lines) and lines[i].strip() != d:
                i += 1
            i += 1
    return '\n'.join(out)

def tokenize(cmd):
    cmd = strip_heredocs(cmd).replace("\\\n", " ").replace("\n", " ; ")
    lx = shlex.shlex(cmd, posix=True, punctuation_chars=';&|()<>')
    lx.whitespace = ' \t\r'
    lx.whitespace_split = True
    toks = []
    try:
        for t in lx: toks.append(t)
    except ValueError:
        return None
    return toks

def segments(toks):
    seg = []
    for t in toks:
        if t in SEPS or t == '\n' or (t and set(t) <= set(';&|()\n')):
            if seg: yield seg
            seg = []
        else:
            seg.append(t)
    if seg: yield seg

def bad(p):
    return not p or any(c in p for c in '$`') or p.startswith('/dev/') or p == '-'

def res(cwd, p):
    p = os.path.expanduser(p)
    return os.path.normpath(os.path.join(cwd, p))

def split_redirs(seg):
    """Return (words, input_redirect_files). Drops output redirects."""
    words, ins, i = [], [], 0
    while i < len(seg):
        t = seg[i]
        if t in ('<',):
            if i + 1 < len(seg): ins.append(seg[i+1])
            i += 2; continue
        if t in ('>', '>>', '<<', '<<<', '>&', '&>', '<>') or (t and set(t) <= set('<>&')):
            i += 2; continue
        if re.fullmatch(r'\d', t) and i + 1 < len(seg) and seg[i+1] and seg[i+1][0] in '<>':
            i += 1; continue
        if re.fullmatch(r'\d?>&?\d*|&>', t):  # e.g. 2>&1 kept as one token sometimes
            i += 1; continue
        words.append(t); i += 1
    return words, ins

PREFIXES = {'sudo','env','command','exec','time','nice','nohup','xargs','timeout','stdbuf'}

def prog_words(words):
    i = 0
    while i < len(words):
        w = words[i]
        if re.fullmatch(r'[A-Za-z_][A-Za-z0-9_]*=.*', w): i += 1; continue
        b = os.path.basename(w)
        if b == 'xargs': return None  # file operands unknown
        if b in PREFIXES:
            i += 1
            if b == 'timeout' and i < len(words): i += 1
            while i < len(words) and words[i].startswith('-'): i += 1
            continue
        if b == 'nix' : return None
        return words[i:]
    return None

def operands(args, valopts, longval=True):
    ops, i, ddash = [], 0, False
    given = []
    while i < len(args):
        a = args[i]
        if ddash: ops.append(a); i += 1; continue
        if a == '--': ddash = True; i += 1; continue
        if a.startswith('--'):
            k = a.split('=')[0]
            if '=' not in a and k in valopts:
                given.append((k, args[i+1] if i+1 < len(args) else None)); i += 2; continue
            given.append((k, a)); i += 1; continue
        if a.startswith('-') and len(a) > 1 and not re.fullmatch(r'-\d+', a):
            # short option cluster; value option may be last letter
            k = a[:2]
            if k in valopts and len(a) == 2:
                given.append((k, args[i+1] if i+1 < len(args) else None)); i += 2; continue
            # cluster like -rn or -n5
            for c in a[1:]:
                given.append(('-' + c, None))
            if a[:2] in valopts: given[-1] = (a[:2], a[2:])
            # clusters ending in value opt e.g. -rne PATTERN
            last = '-' + a[-1]
            if last in valopts and not a[2:].isdigit() and len(a) > 2 and a[:2] not in valopts:
                i += 2; given.append((last, args[i-1] if i-1 < len(args) else None)); continue
            i += 1; continue
        if re.fullmatch(r'[-+]\d+', a): i += 1; continue  # head -5 / tail +3
        ops.append(a); i += 1
    return ops, given

def viewer_files(prog, args):
    b = os.path.basename(prog)
    if b in ('head', 'tail'):
        ops, _ = operands(args, HEAD_VAL); return ops
    if b == 'sed':
        ops, given = operands(args, SED_VAL)
        keys = {k for k, _ in given}
        if '-i' in keys or '--in-place' in keys or any(k.startswith('-i') for k in keys): return []
        if not ({'-e','-f','--expression','--file'} & keys): ops = ops[1:]
        return ops
    if b == 'bat': return operands(args, BAT_VAL)[0]
    if b == 'nl': return operands(args, NL_VAL)[0]
    return operands(args, set())[0]

def search_scopes(prog_args, cwd):
    """Return list of scope paths or None if not a search."""
    b = os.path.basename(prog_args[0]); args = prog_args[1:]
    if b == 'git':
        j = 0
        while j < len(args) and args[j].startswith('-'):
            j += 2 if args[j] in ('-C', '-c') else 1
        if j < len(args) and args[j] == 'grep':
            c = cwd
            if '-C' in args[:j]: c = res(cwd, args[args.index('-C')+1])
            ops, given = operands(args[j+1:], GREP_VAL)
            keys = {k for k, _ in given}
            if not ({'-e','-f'} & keys): ops = ops[1:]
            ops = [o for o in ops if not bad(o) and not re.fullmatch(r'[0-9a-f]{7,40}|HEAD.*|origin/.*', o)]
            return [res(c, o) for o in ops] or [c]
        return None
    if b not in SEARCHERS: return None
    valopts = RG_VAL if b in ('rg', 'ugrep', 'ag', 'ack') else GREP_VAL
    ops, given = operands(args, valopts)
    keys = {k for k, _ in given}
    if not ({'-e','-f','--regexp','--file'} & keys): ops = ops[1:]
    ops = [o for o in ops if not bad(o)]
    recursive = b in ('rg', 'ugrep', 'ag', 'ack') or ({'-r','-R','--recursive','--dereference-recursive'} & keys)
    if ops: return [res(cwd, o) for o in ops]
    if recursive: return [cwd]
    return []  # grep on stdin

def covered(f, scopes):
    for s in scopes:
        if f == s or f.startswith(s.rstrip('/') + '/') or s == '/': return True
    return False

def analyze_file(path):
    st = dict(read=0, bash=0, vreads=0, misses=0, grep_tool=0, bash_search=0,
              read_files=set(), vfiles=set(), miss_files=set(), miss_tmp=0, miss_dir=0, examples=[], miss_by_prog=Counter())
    seen, scopes = set(), []
    with open(path) as fh:
        for line in fh:
            try: r = json.loads(line)
            except Exception: continue
            if r.get('type') != 'assistant': continue
            cwd = r.get('cwd') or '/'
            content = (r.get('message') or {}).get('content')
            if not isinstance(content, list): continue
            for blk in content:
                if not isinstance(blk, dict) or blk.get('type') != 'tool_use': continue
                name, inp = blk.get('name'), blk.get('input') or {}
                if name == 'Read':
                    fp = inp.get('file_path')
                    if fp:
                        f = res(cwd, fp); st['read'] += 1; st['read_files'].add(f); seen.add(f)
                elif name == 'Grep':
                    st['grep_tool'] += 1
                    scopes.append(res(cwd, inp.get('path') or '.'))
                elif name == 'Bash':
                    st['bash'] += 1
                    cmd = inp.get('command') or ''
                    toks = tokenize(cmd)
                    if toks is None: continue
                    c = cwd
                    for seg in segments(toks):
                        words, ins = split_redirs(seg)
                        if words and words[0] in ('cd', 'pushd'):
                            if len(words) > 1 and not bad(words[1]) and words[1] != '-':
                                c = res(c, words[1])
                            elif len(words) == 1: c = os.path.expanduser('~')
                            continue
                        files, prog = [], None
                        pw = prog_words(words) if words else None
                        if pw:
                            prog = os.path.basename(pw[0])
                            sc = search_scopes(pw, c)
                            if sc is not None:
                                if sc or ins: st['bash_search'] += 1
                                scopes.extend(sc)
                                # stdin redirect into a search counts as search of that file
                                scopes.extend(res(c, x) for x in ins if not bad(x))
                                continue
                            if prog in VIEWERS:
                                files += viewer_files(pw[0], pw[1:])
                        files += ins
                        for fp in files:
                            if bad(fp): continue
                            f = res(c, fp)
                            st['vreads'] += 1; st['vfiles'].add(f)
                            if f not in seen and covered(f, scopes):
                                st['misses'] += 1; st['miss_files'].add(f)
                                if f.startswith('/tmp/') or f.endswith('.log'): st['miss_tmp'] += 1
                                if f not in scopes: st['miss_dir'] += 1
                                st['miss_by_prog'][prog or '<'] += 1
                                if len(st['examples']) < 3: st['examples'].append(' '.join(seg)[:100])
                            seen.add(f)
    return st

def main():
    files = glob.glob(BASE + '/*/*.jsonl') + glob.glob(BASE + '/*/*/subagents/*.jsonl')
    per = {}
    for p in files:
        rel = os.path.relpath(p, BASE).split('/')
        proj, sess = rel[0], rel[1].removesuffix('.jsonl')
        per[p] = (proj, sess, 'sub' if 'subagents' in rel else 'main', analyze_file(p))
    def agg(items):
        a = Counter(); df = defaultdict(set); ex = []; mp = Counter()
        n = 0
        for proj, sess, kind, s in items:
            n += 1
            for k in ('read','bash','vreads','misses','grep_tool','bash_search','miss_tmp','miss_dir'): a[k] += s[k]
            for k in ('read_files','vfiles','miss_files'): df[k] |= {(proj, x) for x in s[k]}
            ex += s['examples']; mp += s['miss_by_prog']
        d = a['read'] + a['vreads']
        return (f"transcripts={n} Read={a['read']} (distinct {len(df['read_files'])}) Bash={a['bash']} "
                f"viewer_reads={a['vreads']} (distinct {len(df['vfiles'])}) GrepTool={a['grep_tool']} BashSearch={a['bash_search']} "
                f"MISSES={a['misses']} (distinct {len(df['miss_files'])}) = {100*a['misses']/d if d else 0:.1f}% of {d} deliberate reads"
                f"\n   misses: under /tmp or *.log={a['miss_tmp']}, via a directory-scope search={a['miss_dir']} (rest only via a search naming the file itself)"
                f"\n   miss by prog: {dict(mp)}"), ex
    items = list(per.values())
    print('ALL:', agg(items)[0])
    print('main only:', agg([i for i in items if i[2]=='main'])[0])
    print('subagents only:', agg([i for i in items if i[2]=='sub'])[0])
    byproj = defaultdict(list)
    for i in items: byproj[i[0]].append(i)
    for proj, its in sorted(byproj.items(), key=lambda kv: -sum(x[3]['bash'] for x in kv[1])):
        print(f'\n[{proj}]', agg(its)[0])
    print('\nTop transcripts by misses:')
    top = sorted(per.items(), key=lambda kv: -kv[1][3]['misses'])[:10]
    for p, (proj, sess, kind, s) in top:
        print(f"  {proj}/{sess[:12]} {kind} misses={s['misses']} vreads={s['vreads']} read={s['read']} bash={s['bash']} ex={s['examples'][:2]}")
    if '-v' in sys.argv:
        for p, (proj, sess, kind, s) in per.items():
            for e in s['examples']: print('EX', e)
    prefix = next((a for a in sys.argv[1:] if not a.startswith('-')), None)
    big = [p for p in per if per[p][2]=='main' and prefix and per[p][1].startswith(prefix)]
    if not big:
        return
    big = big[0]
    sid = per[big][1]
    print('\nBiggest main session:', per[big][0], sid)
    mine = [per[k] for k in per if per[k][1]==sid or ('/'+sid+'/subagents/') in k]
    print('  it alone (+ its subagents):', agg(mine)[0])
    print('  everything else:', agg([per[k] for k in per if not (per[k][1]==sid or ('/'+sid+'/subagents/') in k)])[0])


main()
