// Converts $metadata with the OASIS converter, an independent reader of CSDL.
const { execFileSync } = require("child_process");
const fs = require("fs");

(async () => {
  const res = await fetch(`${process.env.ODATA_ROOT}/$metadata`);
  fs.writeFileSync("metadata.xml", await res.text());
  const out = execFileSync("npx", ["--no-install", "odata-openapi3", "metadata.xml", "-t", "openapi.json"], { encoding: "utf8" });
  if (out.trim()) console.log(out.trim());
  const doc = JSON.parse(fs.readFileSync("openapi.json"));
  const paths = Object.keys(doc.paths);
  const expected = ["/BusinessPartners", "/Roles", "/Settings", "/$batch"];
  const missing = expected.filter((p) => !paths.includes(p));
  console.log(`OpenAPI ${doc.openapi}: ${paths.length} paths, ${Object.keys(doc.components.schemas).length} schemas`);
  if (missing.length) { console.log("FAIL  missing", missing); process.exit(1); }
  console.log("ok    the converter understood the metadata");
})();
