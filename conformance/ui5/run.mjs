// Drives SAPUI5's OData V4 model in a real browser. The page is served from the service's
// own origin, as a Fiori app would be, so requests are not cross-origin.
import { chromium } from "playwright";

const root = (process.env.ODATA_ROOT ?? "http://host.docker.internal:3000/odata").replace(/\/$/, "");
const origin = new URL(root).origin;
const page_url = `${origin}/conformance.html`;
const html = `<!doctype html><html><head><script id="sap-ui-bootstrap"
  src="https://sdk.openui5.org/resources/sap-ui-core.js"
  data-sap-ui-libs="sap.ui.core" data-sap-ui-async="true"></script></head><body></body></html>`;

const browser = await chromium.launch();
const page = await browser.newPage({ locale: "en-US" });
const sent = [];
const applied = [];
page.on("request", (r) => {
  if (!r.url().startsWith(root)) return;
  sent.push(`${r.method()} ${r.url().slice(root.length)}`);
  for (const m of (r.postData() ?? "").matchAll(/^GET (\S*apply\S*) HTTP/gm)) applied.push(decodeURIComponent(m[1]));
});
page.on("console", (m) => { if (m.type() === "error") console.log("console:", m.text().slice(0, 300)); });
await page.route(page_url, (r) => r.fulfill({ contentType: "text/html", body: html }));
await page.goto(page_url);
await page.waitForFunction(() => window.sap && sap.ui && sap.ui.require);

const results = await page.evaluate(async (serviceUrl) => {
  const load = (names) => new Promise((ok, no) => sap.ui.require(names, (...m) => ok(m), no));
  const [ODataModel] = await load(["sap/ui/model/odata/v4/ODataModel"]);
  await new Promise((ok) => sap.ui.getCore().attachInit(ok));
  const out = [];
  const check = async (name, run) => {
    try { out.push({ name, ok: true, detail: await run() }); }
    catch (e) { out.push({ name, ok: false, detail: String(e && e.message || e).slice(0, 300) }); }
  };
  const model = new ODataModel({
    serviceUrl: serviceUrl + "/", operationMode: "Server", groupId: "$auto", updateGroupId: "$auto",
    synchronizationMode: "None", autoExpandSelect: false,
  });
  const meta = model.getMetaModel();
  const list = (path, params) => model.bindList(path, undefined, undefined, undefined, params);

  await check("metadata loads in the meta model", async () => {
    const type = await meta.requestObject("/BusinessPartners/$Type");
    const key = await meta.requestObject("/BusinessPartners/$Type/$Key");
    if (type !== "Test.BusinessPartner" || key[0] !== "Id") throw new Error(type + " " + key);
    return type;
  });
  await check("list binding through $batch", async () => {
    const c = await list("/BusinessPartners", { $select: "Id,Name", $orderby: "Name desc" }).requestContexts(0, 10);
    return c.length + " rows, first " + c[0].getObject().Name;
  });
  await check("$filter and $count", async () => {
    const b = list("/BusinessPartners", { $count: true, $filter: "Rating gt 2" });
    const c = await b.requestContexts(0, 1);
    return c.length + " of " + b.getLength() + " / $count " + (await b.requestContexts(0, 10)).length;
  });
  await check("$expand", async () => {
    const c = await list("/BusinessPartners", { $expand: { Roles: { $select: "Code", $filter: "Code gt 1" } } }).requestContexts(0, 2);
    return c[0].getObject().Roles.length + " roles";
  });
  await check("context binding reads an entity with ETag", async () => {
    const o = await model.bindContext("/BusinessPartners('1')").requestObject();
    if (!o["@odata.etag"]) throw new Error("no etag");
    return o["@odata.etag"];
  });
  await check("navigation path", async () => {
    return (await list("/BusinessPartners('1')/Roles").requestContexts(0, 10)).length + " roles";
  });
  await check("property binding", async () => {
    return model.bindProperty("/BusinessPartners('1')/Name").requestValue();
  });
  await check("create and read back", async () => {
    const b = list("/Roles");
    await b.requestContexts(0, 1);
    const ctx = b.create({ PartnerId: "4", Code: 90, Label: "ui5" });
    await ctx.created();
    return ctx.getProperty("Label");
  });
  await check("update through a context (PATCH with If-Match)", async () => {
    const c = await list("/BusinessPartners", { $filter: "Id eq '4'" }).requestContexts(0, 1);
    await c[0].setProperty("Name", "From UI5");
    const o = await model.bindContext("/BusinessPartners('4')").requestObject();
    if (o.Name !== "From UI5") throw new Error(o.Name);
    return o["@odata.etag"];
  });
  await check("delete through a context", async () => {
    const c = await list("/Roles", { $filter: "PartnerId eq '4' and Code eq 90" }).requestContexts(0, 1);
    await c[0].delete();
    return "deleted";
  });
  await check("function import", async () => {
    const f = model.bindContext("/Add(...)");
    f.setParameter("a", 2).setParameter("b", 3);
    await f.invoke();
    return f.getBoundContext().getObject().value;
  });
  await check("action import", async () => {
    const a = model.bindContext("/Sum(...)");
    a.setParameter("a", 4).setParameter("b", 5);
    await a.invoke();
    return a.getBoundContext().getObject().value;
  });
  await check("$apply as a binding parameter", async () => {
    const c = await list("/BusinessPartners", {
      $apply: "groupby((Abc),aggregate(Rating with sum as Total))", $orderby: "Abc",
    }).requestContexts(0, 10);
    const row = c.find((x) => x.getObject().Abc === "A").getObject();
    if (Number(row.Total) !== 5) throw new Error(JSON.stringify(row));
    return c.length + " groups";
  });
  await check("$$aggregation generates $apply", async () => {
    const b = list("/BusinessPartners", {
      $$aggregation: {
        group: { Abc: {} },
        aggregate: { Total: { name: "Rating", with: "sum" } },
      },
      $orderby: "Abc",
    });
    const c = await b.requestContexts(0, 10);
    const row = c.find((x) => x.getObject().Abc === "A").getObject();
    if (Number(row.Total) !== 5) throw new Error(JSON.stringify(row));
    return c.length + " groups";
  });
  await check("$$aggregation with a filter on an aggregate", async () => {
    const b = list("/BusinessPartners", {
      $$aggregation: {
        group: { Active: {} },
        aggregate: { Total: { name: "Rating", with: "sum" } },
      },
      $filter: "Total gt 2",
    });
    return (await b.requestContexts(0, 10)).length + " groups";
  });
  await check("$$aggregation with a count of groups", async () => {
    const b = list("/BusinessPartners", {
      $count: true,
      $$aggregation: { group: { Abc: {} } },
    });
    const c = await b.requestContexts(0, 2);
    await new Promise((r) => setTimeout(r, 200));
    return c.length + " of " + b.getLength();
  });
  await check("$$aggregation with a grand total", async () => {
    const b = list("/BusinessPartners", {
      $$aggregation: {
        group: { Active: {} },
        aggregate: { Total: { grandTotal: true, name: "Rating", with: "sum" } },
      },
      $orderby: "Active desc",
    });
    const c = await b.requestContexts(0, 10);
    const rows = c.map((x) => x.getObject());
    const total = rows.find((r) => r["@$ui5.node.isTotal"]);
    if (!total || Number(total.Total) !== 9) throw new Error(JSON.stringify(rows));
    if (rows.length !== 3 || rows[1].Active !== true) throw new Error(JSON.stringify(rows));
    return rows.length + " rows, total " + total.Total;
  });
  await check("$$aggregation with group levels and subtotals", async () => {
    const b = list("/BusinessPartners", {
      $$aggregation: {
        group: { Name: {} },
        groupLevels: ["Active"],
        aggregate: { Total: { subtotals: true, grandTotal: true, name: "Rating", with: "sum" } },
      },
      $orderby: "Active desc",
    });
    const c = await b.requestContexts(0, 10);
    const active = c.find((x) => x.getObject().Active === true);
    if (Number(active.getObject().Total) !== 6) throw new Error(JSON.stringify(active.getObject()));
    await active.expand();
    await new Promise((r) => setTimeout(r, 300));
    const after = await b.requestContexts(0, 10);
    if (after.length !== c.length + 3) throw new Error(after.length + " rows");
    return c.length + " levels, " + after.length + " rows after expanding";
  });
  await check("entity read failure surfaces as an OData error", async () => {
    try { await model.bindContext("/BusinessPartners('nope')").requestObject(); }
    catch (e) { if (e.status === 404) return e.message; throw e; }
    throw new Error("expected a failure");
  });
  return out;
}, root);

let failed = 0;
for (const r of results) {
  if (!r.ok) failed++;
  console.log(`${r.ok ? "ok   " : "FAIL "} ${r.name}${r.detail ? " -> " + r.detail : ""}`);
}
console.log(`\n${results.length - failed} passed, ${failed} failed`);
console.log("\nrequests:\n  " + [...new Set(sent)].slice(0, 12).join("\n  "));
console.log("\n$apply sent inside batches:\n  " + [...new Set(applied)].join("\n  "));
await browser.close();
process.exit(failed ? 1 : 0);
