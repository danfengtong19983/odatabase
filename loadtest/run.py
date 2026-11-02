#!/usr/bin/env python3
"""Measures the load server over HTTP. Prints a table of latencies, then concurrency results."""
import json
import os
import statistics
import sys
import threading
import time
import urllib.error
import urllib.parse
import urllib.request

ROOT = os.environ.get("ODATA_ROOT", "http://127.0.0.1:3000/odata")
CAP = float(os.environ.get("CAP_SECONDS", "60"))

QUERIES = [
    ("key read", "Customers(12345)"),
    ("first page", "Orders?$top=100"),
    ("indexed filter", "Orders?$filter=CustomerId eq 1234"),
    ("unindexed filter", "Orders?$filter=Total gt 999.9&$top=100"),
    ("string function", "Customers?$filter=contains(Name,'99999')&$top=100"),
    ("order by indexed", "Orders?$orderby=PlacedAt desc&$top=100"),
    ("order by unindexed", "Orders?$orderby=Total desc&$top=100"),
    ("$count segment", "Orders/$count"),
    ("$count=true", "Orders?$top=10&$count=true"),
    ("filtered $count", "Orders/$count?$filter=Status eq 'paid'"),
    ("skip 10k", "Orders?$skip=10000&$top=100"),
    ("skip 4M", "Orders?$skip=4000000&$top=100"),
    ("to-one filter, selective", "Orders?$filter=Customer/Name eq 'Customer 77'&$top=100"),
    ("to-one filter, common", "Orders?$filter=Customer/Country eq 'DE'&$top=100"),
    ("to-one filter, rare", "Orders?$filter=Customer/Country eq 'XX'&$top=100"),
    ("to-one order", "Orders?$orderby=Customer/Name&$top=100"),
    ("any, common", "Customers?$filter=Orders/any(o: o/Total gt 500)&$top=100"),
    ("any, rare", "Customers?$filter=Orders/any(o: o/Total gt 999.99)&$top=100"),
    ("all", "Customers?$filter=Orders/all(o: o/Total gt 50)&$top=100"),
    ("nav $count filter", "Customers?$filter=Orders/$count gt 25&$top=100"),
    ("nav $count order", "Customers?$orderby=Orders/$count desc&$top=100"),
    ("expand to-many, 100", "Customers?$top=100&$expand=Orders"),
    ("expand to-many, top 3", "Customers?$top=100&$expand=Orders($top=3;$orderby=PlacedAt desc)"),
    ("expand to-one, 1000", "Orders?$top=1000&$expand=Customer"),
    ("expand with count", "Customers?$top=100&$expand=Orders($count=true;$top=1)"),
    ("expand nested", "Customers?$top=20&$expand=Orders($expand=Customer)"),
    ("apply sum", "Orders?$apply=aggregate(Total with sum as T)"),
    ("apply groupby column", "Orders?$apply=groupby((Status),aggregate(Total with sum as T))"),
    ("apply groupby to-one", "Orders?$apply=groupby((Customer/Country),aggregate(Total with sum as T))"),
    ("apply groupby many keys", "Orders?$apply=groupby((CustomerId),aggregate(Total with sum as T))&$top=100"),
    ("apply year", "Orders?$apply=compute(year(PlacedAt) as Y)/groupby((Y),aggregate($count as N))"),
    ("apply having", "Orders?$apply=groupby((CustomerId),aggregate(Total with sum as T))/filter(T gt 20000)&$top=100"),
]


def fetch(path):
    url = ROOT + "/" + urllib.parse.quote(path, safe="/?&=$(),':;")
    start = time.perf_counter()
    try:
        with urllib.request.urlopen(url, timeout=CAP) as r:
            body = r.read()
            return r.status, time.perf_counter() - start, len(body)
    except urllib.error.HTTPError as e:
        return e.code, time.perf_counter() - start, 0
    except Exception:
        return 0, time.perf_counter() - start, 0


def latency():
    rows = []
    print(f"{'query':<28} {'status':>6} {'best ms':>10} {'median ms':>10} {'KB':>8}")
    for name, path in QUERIES:
        results = []
        for _ in range(3):
            r = fetch(path)
            results.append(r)
            if r[1] > 5 or r[0] != 200:
                break
        times = [r[1] * 1000 for r in results]
        status = results[-1][0]
        size = results[-1][2] / 1024
        label = "cap" if status == 0 else status
        print(f"{name:<28} {label!s:>6} {min(times):>10.1f} {statistics.median(times):>10.1f} {size:>8.1f}", flush=True)
        rows.append({"name": name, "status": status, "best_ms": min(times), "median_ms": statistics.median(times)})
    return rows


def concurrent(paths, workers, seconds):
    stop = time.perf_counter() + seconds
    times, errors = [], [0]
    lock = threading.Lock()

    def work(n):
        i = n
        while time.perf_counter() < stop:
            status, elapsed, _ = fetch(paths[i % len(paths)])
            i += 1
            with lock:
                if status == 200:
                    times.append(elapsed * 1000)
                else:
                    errors[0] += 1

    threads = [threading.Thread(target=work, args=(n,)) for n in range(workers)]
    for t in threads:
        t.start()
    for t in threads:
        t.join()
    times.sort()
    if not times:
        return workers, 0, 0, 0, errors[0]
    p = lambda q: times[min(len(times) - 1, int(len(times) * q))]
    return workers, len(times) / seconds, p(0.5), p(0.99), errors[0]


def main():
    mode = sys.argv[1] if len(sys.argv) > 1 else "all"
    if mode in ("all", "latency"):
        latency()
    if mode in ("all", "concurrency"):
        mixes = {
            "key reads": [f"Customers({i})" for i in range(1, 400)],
            "page + expand": ["Customers?$top=50&$expand=Orders($top=3)"],
        }
        print(f"\n{'mix':<16} {'workers':>7} {'req/s':>9} {'p50 ms':>9} {'p99 ms':>9} {'errors':>7}")
        for name, paths in mixes.items():
            for workers in (1, 8, 32, 64):
                w, rate, p50, p99, errors = concurrent(paths, workers, 8)
                print(f"{name:<16} {w:>7} {rate:>9.0f} {p50:>9.1f} {p99:>9.1f} {errors:>7}", flush=True)


main()
