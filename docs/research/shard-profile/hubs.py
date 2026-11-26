"""Pick realistic Verb arguments from one graph segment (graph.sqlite).

Returns Shard-local ids (`sym:path#name`, `file:path`, ...); the Verb bench
turns them into public ids by inserting the repo qualifier after the kind
prefix. Used by harness.py while the local segment still exists.
"""

import sqlite3

DEGREE_SQL = """
with deg as (
  select id, sum(n) as degree from (
    select src as id, count(*) as n from edges group by src
    union all
    select dst as id, count(*) as n from edges group by dst
  ) group by id
)
select n.id, n.kind, n.name, deg.degree from deg join nodes n on n.id = deg.id
{where} order by deg.degree desc, n.id limit {limit}
"""


def _rows(conn, sql, args=()):
    return [list(r) for r in conn.execute(sql, args)]


def pick(graph_path):
    conn = sqlite3.connect(f"file:{graph_path}?mode=ro", uri=True)
    try:
        top_any = _rows(conn, DEGREE_SQL.format(where="", limit=5))
        top_symbol = _rows(conn, DEGREE_SQL.format(where="where n.kind = 'Symbol'", limit=1))
        top_file = _rows(conn, DEGREE_SQL.format(where="where n.kind = 'File'", limit=1))
        top_package = _rows(conn, DEGREE_SQL.format(where="where n.kind = 'Package'", limit=1))
        hot_file = _rows(conn, (
            "select dst, count(*) from edges where kind = 'TOUCHES'"
            " group by dst order by count(*) desc, dst limit 1"))
        # A typical symbol: the median-degree symbol with a unique name.
        median_symbol = _rows(conn, (
            "with d as (select src as id, count(*) as degree from edges group by src),"
            " u as (select name from nodes where kind = 'Symbol' group by name having count(*) = 1),"
            " s as (select n.id, n.name, coalesce(d.degree, 0) as degree from nodes n"
            "   join u on u.name = n.name left join d on d.id = n.id where n.kind = 'Symbol')"
            " select id, name, degree from s order by degree, id"
            " limit 1 offset (select count(*) / 2 from s)"))
        common_name = _rows(conn, (
            "select name, count(*) from nodes where kind = 'Symbol' and length(name) > 2"
            " group by name order by count(*) desc, name limit 1"))
        kinds = _rows(conn, "select kind, count(*) from edges group by kind order by kind")
    finally:
        conn.close()
    return {
        "top_any": top_any,
        "top_symbol": top_symbol[0] if top_symbol else None,
        "top_file": top_file[0] if top_file else None,
        "top_package": top_package[0] if top_package else None,
        "hot_file": hot_file[0] if hot_file else None,
        "median_symbol": median_symbol[0] if median_symbol else None,
        "common_name": common_name[0] if common_name else None,
        "edge_kinds": kinds,
    }
