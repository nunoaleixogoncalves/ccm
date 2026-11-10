# Records the invented "Never answered" and "Feature branch work" conversations as started, so ccm
# shows them as crashed.
import glob, json, os, sys, time
h=sys.argv[1]; now=int(time.time()*1000)-120000
want=[]
for p in glob.glob(h+"/.claude/projects/*/*.jsonl"):
    t=open(p).read()
    if '"Never answered"' in t or '"Feature branch work"' in t: want.append(os.path.basename(p)[:-6])
with open(h+"/.local/state/ccm/sessions.jsonl","w") as f:
    for i in want: f.write(json.dumps({"event":"start","id":i,"ts":now})+"\n")
