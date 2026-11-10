# Invented conversations and worktrees for the worktrees suite. Nothing here comes from a real conversation.
# Invented HOME for the worktree and clean-up screens. Nothing comes from a real conversation.
import json, os, shutil, subprocess, sys, datetime as dt, uuid, glob, time
root=os.path.abspath(sys.argv[1]); home=root+"/home"
shutil.rmtree(root,ignore_errors=True); os.makedirs(home)
env={**os.environ,"HOME":home,"GIT_AUTHOR_NAME":"t","GIT_AUTHOR_EMAIL":"t@example.invalid","GIT_COMMITTER_NAME":"t","GIT_COMMITTER_EMAIL":"t@example.invalid"}
git=lambda cwd,*a: subprocess.run(["git",*a],cwd=cwd,env=env,check=True,capture_output=True)
repo=home+"/work/shop"; os.makedirs(repo)
git(repo,"init","-b","main"); open(repo+"/a","w").write("a"); git(repo,"add","a"); git(repo,"commit","-m","one")
def wt(name):
    p=home+"/work/"+name; git(repo,"worktree","add","-b",name,p); return p
tidy=wt("shop-tidy"); open(tidy+"/payload","w").write("x"*40000); git(tidy,"add","payload"); git(tidy,"commit","-m","p"); git(repo,"merge","--ff-only","shop-tidy")
tidy2=wt("shop-tidy2")
wip=wt("shop-wip"); open(wip+"/dirty","w").write("d")
ahead=wt("shop-ahead"); open(ahead+"/b","w").write("b"); git(ahead,"add","b"); git(ahead,"commit","-m","two")
worked=wt("shop-worked")
ign=wt("shop-ignored"); open(repo+"/.git/info/exclude","a").write("\nsecret.env\n"); open(ign+"/secret.env","w").write("s"*3000)
lock=wt("shop-locked"); git(repo,"worktree","lock",lock)
gone=wt("shop-gone"); shutil.rmtree(gone)
now=dt.datetime.now(dt.timezone.utc)
def ts(mins): return (now-dt.timedelta(minutes=mins)).strftime("%Y-%m-%dT%H:%M:%S.000Z")
def claude(folder,name,mins,pad=0,replyless=False):
    sid=str(uuid.uuid4()); d=home+"/.claude/projects/"+folder.replace("/","-"); os.makedirs(d,exist_ok=True)
    u={"type":"user","cwd":folder,"gitBranch":"main","timestamp":ts(mins+1),"message":{"role":"user","content":"invented prompt"}}
    a={"type":"assistant","cwd":folder,"gitBranch":"main","timestamp":ts(mins),"message":{"id":"m"+sid,"model":"claude-test-5","content":[{"type":"text","text":"invented reply"}],"usage":{"input_tokens":10,"output_tokens":9}}}
    with open(f"{d}/{sid}.jsonl","w") as f:
        for l in [{"type":"custom-title","customTitle":name},u]+([] if replyless else [a]): f.write(json.dumps(l)+"\n")
        if pad: f.write(json.dumps({"type":"progress","data":"x"*pad})+"\n")
    return sid
plain=home+"/notes"; os.makedirs(plain)
claude(repo,"Main repo conversation",12)
claude(worked,"Worked in a worktree",40)
claude(wip,"Dirty worktree chat",50)
claude(ahead,"Ahead worktree chat",60)
claude(lock,"Locked worktree chat",70)
claude(ign,"Ignored files chat",75)
claude(plain,"Never answered A",500,replyless=True)
claude(plain,"Never answered B",510,replyless=True)
claude(home+"/deleted-a","Folder gone A",800)
claude(plain,"Huge old conversation",60*24*90,pad=120_000_000)
claude(plain,"Recent big one",60*24,pad=120_000_000)
for i in range(120): claude(plain,f"Filler {i:03d}",2000+i)
for p in glob.glob(home+"/.claude/projects/*/*.jsonl"):
    old=time.time()-3600; os.utime(p,(old,old))
# Job folders.
jobs=home+"/.claude/jobs"; os.makedirs(jobs)
def job(short,state):
    d=jobs+"/"+short; os.makedirs(d)
    if state is not None: json.dump(state,open(d+"/state.json","w"))
    open(d+"/log","w").write("l"*2048)
    old=time.time()-3*86400
    for x in [d+"/log",d+"/state.json",d]:
        if os.path.exists(x): os.utime(x,(old,old))
job("aaaaaaaa",{"sessionId":"00000000-dead-4000-8000-000000000001","bgIsolation":"none","name":"A job with no conversation"})
job("bbbbbbbb",{"sessionId":"00000000-dead-4000-8000-000000000002","bgIsolation":"worktree","name":"A job that may have a worktree"})
