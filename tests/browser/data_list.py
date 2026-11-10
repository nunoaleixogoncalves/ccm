# Invented conversations for the list and actions suites. Nothing here comes from a real conversation.
# Builds an invented HOME: nothing here comes from a real conversation.
import json, os, shutil, subprocess, sys, datetime as dt, uuid
root=os.path.abspath(sys.argv[1]); home=root+"/home"
shutil.rmtree(root,ignore_errors=True); os.makedirs(home)
env={**os.environ,"HOME":home,"GIT_AUTHOR_NAME":"t","GIT_AUTHOR_EMAIL":"t@example.invalid","GIT_COMMITTER_NAME":"t","GIT_COMMITTER_EMAIL":"t@example.invalid"}
git=lambda cwd,*a: subprocess.run(["git",*a],cwd=cwd,env=env,check=True,capture_output=True)
repo=home+"/work/shop"; os.makedirs(repo)
git(repo,"init","-b","main"); open(repo+"/a","w").write("a"); git(repo,"add","a"); git(repo,"commit","-m","one")
wt=home+"/work/shop-feature"; git(repo,"worktree","add","-b","feature",wt)
plain=home+"/notes"; os.makedirs(plain)
now=dt.datetime.now(dt.timezone.utc)
def ts(mins): return (now-dt.timedelta(minutes=mins)).strftime("%Y-%m-%dT%H:%M:%S.000Z")
def claude(folder,name,prompt,reply,mins,branch="main",model="claude-test-5",extra=[],pad=0,replyless=False):
    sid=str(uuid.uuid4()); d=home+"/.claude/projects/"+folder.replace("/","-"); os.makedirs(d,exist_ok=True)
    u={"type":"user","cwd":folder,"gitBranch":branch,"timestamp":ts(mins+1),"message":{"role":"user","content":prompt}}
    a={"type":"assistant","cwd":folder,"gitBranch":branch,"timestamp":ts(mins),"message":{"id":"m"+sid,"model":model,"content":[{"type":"text","text":reply},{"type":"tool_use","id":"t","name":"Read","input":{}}],"usage":{"input_tokens":10,"cache_read_input_tokens":80000,"cache_creation_input_tokens":7000,"output_tokens":99}}}
    lines=[{"type":"custom-title","customTitle":name},u]+([] if replyless else [a])+extra
    with open(f"{d}/{sid}.jsonl","w") as f:
        for l in lines: f.write(json.dumps(l)+"\n")
        if pad: f.write(json.dumps({"type":"progress","data":"x"*pad})+"\n")
    return sid
long="\n".join(f"Paragraph {i} of a long invented reply about the checkout flow." for i in range(60))
claude(repo,"Fix the checkout total","The total is wrong when a coupon applies","Rounded after the discount now.\n"+long,12)
claude(wt,"Feature branch work","Add the zebra pricing tier","Added it.",40,branch="feature")
claude(plain,"<img src=x onerror=\"window.pwned=1\"><script>window.pwned=2</script>","<b>bold?</b> <img src=x onerror=\"window.pwned=3\">","<script>window.pwned=4</script> & done",200)
claude(plain,"\u202eevil\x1b[31m hidden name","control characters","ok",300)
claude(plain,"Huge old conversation","big","ok",60*24*90,pad=250_000_000)
claude(plain,"Never answered","hello?","",500,replyless=True)
gone=home+"/deleted-project"; claude(gone,"Folder was removed","x","y",800)
# Codex
cid="019a5a1e-7c3b-7d21-9f4e-2b6c1d8e0a11"
d=home+"/.codex/sessions/2026/11/05"; os.makedirs(d)
L=[{"timestamp":ts(30),"type":"session_meta","payload":{"session_id":cid,"id":cid,"timestamp":ts(30),"cwd":repo,"originator":"codex-tui","cli_version":"0.167.0","source":"cli","model_provider":"openai","git":{"branch":"main"}}},
{"timestamp":ts(30),"type":"turn_context","payload":{"turn_id":"t1","cwd":repo,"model":"gpt-5.5-codex"}},
{"timestamp":ts(30),"type":"event_msg","payload":{"type":"task_started","turn_id":"t1","model_context_window":272000}},
{"timestamp":ts(30),"type":"event_msg","payload":{"type":"user_message","message":"Fix the failing parser test","local_images":[]}},
{"timestamp":ts(29),"type":"response_item","payload":{"type":"function_call","name":"shell","arguments":"{}","call_id":"c1"}},
{"timestamp":ts(29),"type":"event_msg","payload":{"type":"agent_message","message":"Fixed the off-by-one in parse_header.","phase":"final_answer"}},
{"timestamp":ts(29),"type":"event_msg","payload":{"type":"token_count","info":{"last_token_usage":{"input_tokens":12034,"output_tokens":412,"total_tokens":12446},"model_context_window":272000}}}]
open(f"{d}/rollout-2026-11-05T14-03-12-{cid}.jsonl","w").write("\n".join(json.dumps(x) for x in L)+"\n")
shutil.rmtree(gone,ignore_errors=True)
