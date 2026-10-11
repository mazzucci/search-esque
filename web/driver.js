// The driver the page and the parity harness share: one statement in, its rows out, as a JSON
// string, for the crate's executor running in WebAssembly. Parameters arrive typed (null,
// {int}, {real}, {text}): a real is bound with sqlite3_bind_double, because the JavaScript
// number 10 would otherwise be bound as an integer, which divides and compares differently,
// and a huge whole number would wrap. Errors come back as {"error"} so the crate can report
// them with a path.
export function makeRunner(sqlite3, db) {
  return function runStatement(sql, paramsJson) {
    const params = JSON.parse(paramsJson);
    let stmt = null;
    try {
      stmt = db.prepare(sql);
      params.forEach((p, i) => {
        const idx = i + 1;
        if (p === null) stmt.bind(idx, null);
        else if ('real' in p) {
          const rc = sqlite3.capi.sqlite3_bind_double(stmt.pointer, idx, p.real);
          if (rc) throw new Error(`cannot bind a real at ${idx} (SQLite error ${rc})`);
        } else if ('int' in p) stmt.bind(idx, Number.isSafeInteger(p.int) ? p.int : BigInt(p.int));
        else stmt.bind(idx, p.text);
      });
      const rows = [];
      while (stmt.step()) rows.push(stmt.get([]));
      // integers beyond 2^53 arrive as BigInt, which JSON cannot carry
      return JSON.stringify({ rows }, (_, v) => (typeof v === 'bigint' ? Number(v) : v));
    } catch (e) {
      return JSON.stringify({ error: String(e && e.message ? e.message : e) });
    } finally {
      if (stmt) stmt.finalize();
    }
  };
}

// After a failure the connection must not be left inside a transaction.
export function settle(sqlite3, db) {
  try {
    if (db && sqlite3.capi.sqlite3_get_autocommit(db.pointer) === 0) db.exec('ROLLBACK');
  } catch (e) { /* nothing to roll back */ }
}
