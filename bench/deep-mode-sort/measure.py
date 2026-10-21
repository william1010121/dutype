from pathlib import Path
import argparse, json, statistics, subprocess, sys, time
parser=argparse.ArgumentParser(description='Measure one bounded sort_tree matrix; no performance threshold')
parser.add_argument('--output', type=Path, required=True)
args=parser.parse_args()
if not sys.platform.startswith("linux"):
    parser.error("RSS measurement requires Linux")
out=args.output.resolve()
results=out/'benchmark-results'
results.mkdir(exist_ok=False)
start=time.monotonic()
def run(args):
    remaining = 60 - (time.monotonic() - start)
    if remaining <= 0:
        raise TimeoutError("the bounded measurement window has ended")
    return subprocess.run(args, check=True, text=True, capture_output=True, timeout=remaining)
def host(label):
    (results/f'{label}-host.txt').write_text(run(['sh','-c','date -u; uptime; ps -eo pid,ppid,stat,pcpu,pmem,comm --sort=-pcpu | head -25; df -h .']).stdout)
host('before')
raw=[]
summary={}
for k,case in enumerate(['nonclone','ordinary','wide-files','wide-dirs','deep']):
    summary[case]={}
    for variant in (['before','after'] if k%2==0 else ['after','before']):
        command=[str(out/f'benchmark-{variant}'),case,'time']
        data=json.loads(run(command).stdout)
        raw.append({'case':case,'variant':variant,'kind':'time','command':command,'data':data})
        values=data['samples_ns']
        stats={'median_pair_ns':statistics.median(values),'min_pair_ns':min(values),'max_pair_ns':max(values),'pairs_per_sample':data['pairs_per_sample'], 'digests':{field:data[field] for field in ['before','dedup','du']}}
        rss=[]
        for _ in range(3):
            command=[str(out/'rss_exec'),str(out/f'benchmark-{variant}'),case,'memory']
            rss_run=run(command)
            memory={'result':json.loads(rss_run.stdout), **json.loads(rss_run.stderr)}
            raw.append({'case':case,'variant':variant,'kind':'rss','command':command,'data':memory})
            assert all(memory['result'][f]==data[f] for f in ['before','dedup','du'])
            rss.append(memory['max_rss_kib'])
        stats['rss_kib']=rss
        summary[case][variant]=stats
    command=[str(out/'benchmark-frames'),case,'memory']
    frames=json.loads(run(command).stdout)
    raw.append({'case':case,'variant':'frames','kind':'capacity','command':command,'data':frames})
    assert summary[case]['before']['digests']==summary[case]['after']['digests']
    assert all(frames[f]==summary[case]['after']['digests'][f] for f in ['before','dedup','du'])
    summary[case]['after']['peak_iterator_capacity_bytes']=frames['peak_iterator_capacity_bytes']
    summary[case]['ratio']=summary[case]['after']['median_pair_ns']/summary[case]['before']['median_pair_ns']
    (results/'raw.ndjson').write_text(''.join(json.dumps(v)+'\n' for v in raw))
    (results/'summary.json').write_text(json.dumps(summary,indent=2)+'\n')
host('after')
(results/'run.json').write_text(json.dumps({'elapsed_s':time.monotonic()-start,'timestamp_utc':time.strftime('%Y-%m-%dT%H:%M:%SZ',time.gmtime()),'status':'passed','method':'15 samples after 3 warmups; each sample averages N dedup/du toggle pairs; sequential alternating variant order; 3 fresh-child RSS samples, includes whole process; separate frame-capacity binary.'},indent=2)+'\n')
print(json.dumps(summary,indent=2))
