// Reads the service with Microsoft.OData.Core, which validates payloads against $metadata.
using System.Net.Http.Headers;
using System.Text;
using System.Xml;
using Microsoft.OData;
using Microsoft.OData.Edm;
using Microsoft.OData.Edm.Csdl;
using Microsoft.OData.Edm.Validation;

var root = (Environment.GetEnvironmentVariable("ODATA_ROOT") ?? "http://host.docker.internal:3000/odata").TrimEnd('/');
var http = new HttpClient();
int passed = 0, failed = 0;

void Report(string name, Exception e = null)
{
    if (e == null) { passed++; Console.WriteLine($"ok    {name}"); return; }
    failed++;
    Console.WriteLine($"FAIL  {name}\n      {e.GetType().Name}: {e.Message.Split('\n')[0]}");
}

async Task<Response> Send(HttpMethod method, string url, string body = null, string contentType = null, string accept = "application/json")
{
    var req = new HttpRequestMessage(method, url.StartsWith("http") ? url : $"{root}/{url}");
    req.Headers.Accept.ParseAdd(accept);
    if (body != null)
    {
        req.Content = new ByteArrayContent(Encoding.UTF8.GetBytes(body));
        req.Content.Headers.TryAddWithoutValidation("Content-Type", contentType ?? "application/json");
    }
    var res = await http.SendAsync(req);
    return new Response(res, await res.Content.ReadAsByteArrayAsync());
}

// Metadata
IEdmModel model;
{
    var r = await Send(HttpMethod.Get, "$metadata", accept: "application/xml");
    using var xml = XmlReader.Create(new MemoryStream(r.Body));
    if (!CsdlReader.TryParse(xml, ignoreUnexpectedAttributesAndElements: false, out model, out var errors))
    {
        foreach (var e in errors) Console.WriteLine($"FAIL  csdl: {e.ErrorCode} {e.ErrorMessage} @ {e.ErrorLocation}");
        Environment.Exit(1);
    }
    Report("$metadata parses");
    if (model.Validate(out var issues)) Report("$metadata passes EDM validation");
    else foreach (var i in issues) Report("edm validation", new Exception($"{i.ErrorCode}: {i.ErrorMessage} @ {i.ErrorLocation}"));
}

ODataMessageReaderSettings Settings() => new() { BaseUri = new Uri(root + "/") };

async Task ReadFeed(string url)
{
    try
    {
        var r = await Send(HttpMethod.Get, url);
        r.Ensure(url);
        using var mr = new ODataMessageReader((IODataResponseMessage)r, Settings(), model);
        var (set, type) = Resolve(url);
        var reader = mr.CreateODataResourceSetReader((IEdmEntitySetBase)set, type);
        int items = 0;
        while (reader.Read()) if (reader.State == ODataReaderState.ResourceEnd) items++;
        Report($"{url} ({items} resources)");
    }
    catch (Exception e) { Report(url, e); }
}

async Task ReadEntity(string url)
{
    try
    {
        var r = await Send(HttpMethod.Get, url);
        r.Ensure(url);
        using var mr = new ODataMessageReader((IODataResponseMessage)r, Settings(), model);
        var (set, type) = Resolve(url);
        var reader = mr.CreateODataResourceReader(set, type);
        while (reader.Read()) { }
        Report(url);
    }
    catch (Exception e) { Report(url, e); }
}

async Task ReadProperty(string url, string typeName)
{
    try
    {
        var r = await Send(HttpMethod.Get, url);
        r.Ensure(url);
        using var mr = new ODataMessageReader((IODataResponseMessage)r, Settings(), model);
        var type = EdmCoreModel.Instance.GetPrimitive(EdmCoreModel.Instance.GetPrimitiveTypeKind(typeName), true);
        var p = mr.ReadProperty(type);
        Report($"{url} = {p.Value}");
    }
    catch (Exception e) { Report(url, e); }
}

// Walks the path through navigation properties and casts to the set and type it addresses.
(IEdmNavigationSource, IEdmEntityType) Resolve(string url)
{
    var parts = url.Split('?')[0].Split('/').Select(p => p.Split('(')[0]).ToArray();
    IEdmNavigationSource source = (IEdmNavigationSource)model.EntityContainer.FindEntitySet(parts[0])
        ?? model.EntityContainer.FindSingleton(parts[0]);
    var type = source.EntityType;
    foreach (var part in parts.Skip(1))
    {
        if (part.Contains('.')) { type = (IEdmEntityType)model.FindType(part); continue; }
        var nav = type.FindProperty(part) as IEdmNavigationProperty;
        source = source.FindNavigationTarget(nav);
        type = nav.ToEntityType();
    }
    return (source, type);
}

IEdmEntitySet ResolveSet(string url) => (IEdmEntitySet)Resolve(url).Item1;

// Service document
try
{
    var r = await Send(HttpMethod.Get, "");
    using var mr = new ODataMessageReader((IODataResponseMessage)r, Settings(), model);
    var doc = mr.ReadServiceDocument();
    Report($"service document ({doc.EntitySets.Count()} sets, {doc.Singletons.Count()} singletons, {doc.FunctionImports.Count()} functions)");
}
catch (Exception e) { Report("service document", e); }

// Collections
foreach (var url in new[] {
    "BusinessPartners",
    "BusinessPartners?$filter=Rating gt 2&$orderby=Name desc&$top=2&$skip=0&$count=true",
    "BusinessPartners?$select=Name,Address/City",
    "BusinessPartners?$select=Id,Rating",
    "BusinessPartners?$expand=Roles",
    "BusinessPartners?$expand=Roles($select=Code;$top=1;$count=true)",
    "BusinessPartners?$expand=Roles/$count",
    "BusinessPartners?$expand=Roles/$ref",
    "BusinessPartners?$expand=Owner($select=Name),Projects",
    "BusinessPartners/Test.Person",
    "BusinessPartners/Test.Person?$select=FirstName",
    "BusinessPartners('1')/Roles",
    "BusinessPartners('1')/Projects?$expand=Partners",
    "BusinessPartners('1')/Owned",
    "Roles?$expand=Partner",
    "Projects?$expand=Partners($select=Name;$orderby=Id desc)",
    "Divisions?$expand=Children($levels=2)",
}) await ReadFeed(url);

// Entities
foreach (var url in new[] {
    "BusinessPartners('1')",
    "BusinessPartners('1')?$select=Name",
    "BusinessPartners('1')?$expand=Roles,Owner,Projects",
    "BusinessPartners('3')",
    "Roles(PartnerId='1',Code=2)",
    "Roles(PartnerId='1',Code=2)?$expand=Partner",
    "BusinessPartners('3')/Test.Person",
    "Roles(PartnerId='1',Code=2)/Partner",
    "Settings",
}) await ReadEntity(url);

// Full metadata
try
{
    var r = await Send(HttpMethod.Get, "BusinessPartners?$top=2", accept: "application/json;odata.metadata=full");
    using var mr = new ODataMessageReader((IODataResponseMessage)r, Settings(), model);
    var set = ResolveSet("BusinessPartners");
    var reader = mr.CreateODataResourceSetReader(set, set.EntityType);
    while (reader.Read()) { }
    Report("odata.metadata=full feed");
}
catch (Exception e) { Report("odata.metadata=full feed", e); }

// Primitive properties
await ReadProperty("BusinessPartners('1')/Name", "Edm.String");
await ReadProperty("BusinessPartners('1')/Rating", "Edm.Int32");
await ReadProperty("Add(a=2,b=3)", "Edm.Int32");

// Complex and collection properties
async Task ReadValue(string url, Func<ODataMessageReader, object> read)
{
    try
    {
        var r = await Send(HttpMethod.Get, url);
        r.Ensure(url);
        using var mr = new ODataMessageReader((IODataResponseMessage)r, Settings(), model);
        read(mr);
        Report(url);
    }
    catch (Exception e) { Report(url, e); }
}

var partner = (IEdmEntityType)model.FindType("Test.BusinessPartner");
var address = (IEdmComplexType)model.FindType("Test.Address");
await ReadValue("BusinessPartners('1')/Address", mr =>
{
    var reader = mr.CreateODataResourceReader(address);
    while (reader.Read()) { }
    return null;
});
await ReadValue("BusinessPartners('1')/Comments", mr =>
{
    var reader = mr.CreateODataCollectionReader(EdmCoreModel.Instance.GetString(false));
    while (reader.Read()) { }
    return null;
});
await ReadValue("BusinessPartners('1')/Phones", mr =>
{
    var reader = mr.CreateODataResourceSetReader(model.FindType("Test.Phone") as IEdmStructuredType);
    while (reader.Read()) { }
    return null;
});
await ReadValue("BusinessPartners('1')/Abc", mr => mr.ReadProperty(new EdmEnumTypeReference((IEdmEnumType)model.FindType("Test.Abc"), true)));
await ReadValue("BusinessPartners('1')/Revenue", mr => mr.ReadProperty(EdmCoreModel.Instance.GetDecimal(true)));
await ReadValue("BusinessPartners('1')/Founded", mr => mr.ReadProperty(EdmCoreModel.Instance.GetDate(true)));
await ReadValue("BusinessPartners('1')/CreatedAt", mr => mr.ReadProperty(EdmCoreModel.Instance.GetDateTimeOffset(true)));
await ReadValue("BusinessPartners('1')/ExternalId", mr => mr.ReadProperty(EdmCoreModel.Instance.GetGuid(true)));

// A payload written by the library, posted back
try
{
    var set = model.EntityContainer.FindEntitySet("Roles");
    var ms = new MemoryStream();
    var msg = new RequestMessage(ms);
    var ws = new ODataMessageWriterSettings { BaseUri = new Uri(root + "/"), ODataUri = new ODataUri { ServiceRoot = new Uri(root + "/") } };
    using (var mw = new ODataMessageWriter((IODataRequestMessage)msg, ws, model))
    {
        var w = mw.CreateODataResourceWriter(set, set.EntityType());
        w.WriteStart(new ODataResource { Properties = new[] {
            new ODataProperty { Name = "PartnerId", Value = "4" },
            new ODataProperty { Name = "Code", Value = 77 },
            new ODataProperty { Name = "Label", Value = "written by Microsoft.OData" } } });
        w.WriteEnd();
    }
    var r = await Send(HttpMethod.Post, "Roles", Encoding.UTF8.GetString(ms.ToArray()), "application/json");
    r.Ensure("POST Roles", 201);
    using var mr = new ODataMessageReader((IODataResponseMessage)r, Settings(), model);
    var reader = mr.CreateODataResourceReader(set, set.EntityType);
    while (reader.Read()) { }
    Report("POST with a library-written payload, read the response");
}
catch (Exception e) { Report("POST with a library-written payload", e); }

// Batch written and read by the library
try
{
    var ms = new MemoryStream();
    var boundary = "batch_" + Guid.NewGuid();
    var msg = new RequestMessage(ms);
    msg.SetHeader("Content-Type", $"multipart/mixed;boundary={boundary}");
    var ws = new ODataMessageWriterSettings { BaseUri = new Uri(root + "/") };
    using (var mw = new ODataMessageWriter((IODataRequestMessage)msg, ws, model))
    {
        var b = mw.CreateODataBatchWriter();
        b.WriteStartBatch();
        var get = b.CreateOperationRequestMessage("GET", new Uri(root + "/BusinessPartners('1')"), null);
        get.SetHeader("Accept", "application/json");
        b.WriteStartChangeset();
        var post = b.CreateOperationRequestMessage("POST", new Uri(root + "/Roles"), "1");
        post.SetHeader("Content-Type", "application/json");
        using (var s = post.GetStream()) s.Write(Encoding.UTF8.GetBytes("{\"PartnerId\":\"4\",\"Code\":78}"));
        b.WriteEndChangeset();
        b.WriteEndBatch();
    }
    var r = await Send(HttpMethod.Post, "$batch", Encoding.UTF8.GetString(ms.ToArray()), $"multipart/mixed;boundary={boundary}", "multipart/mixed");
    r.Ensure("$batch");
    using var mr = new ODataMessageReader((IODataResponseMessage)r, Settings(), model);
    var br = mr.CreateODataBatchReader();
    int parts = 0;
    var statuses = new List<int>();
    while (br.Read())
        if (br.State == ODataBatchReaderState.Operation)
        {
            var op = br.CreateOperationResponseMessage();
            statuses.Add(op.StatusCode);
            parts++;
        }
    if (parts != 2 || statuses[0] != 200 || statuses[1] != 201) throw new Exception($"statuses {string.Join(",", statuses)}");
    Report("multipart $batch written and read by the library");
}
catch (Exception e) { Report("multipart $batch", e); }

// $apply: Microsoft's own grammar must accept each pipeline, and the server must answer it.
foreach (var query in new[] {
    "aggregate(Rating with sum as Total)",
    "aggregate(Rating with average as Mean,Rating with min as Low,Rating with max as High)",
    "aggregate(Rating with countdistinct as Kinds,$count as Rows)",
    "aggregate(Rating mul 2 with sum as Twice)",
    "groupby((Abc))",
    "groupby((Address/City,Active),aggregate($count as Rows))",
    "groupby((Owner/Name),aggregate(Rating with sum as Total))",
    "filter(Active)/groupby((Address/City),aggregate(Revenue with sum as Total))/filter(Total gt 20)",
    "groupby((Abc),aggregate($count as Rows))/groupby((Rows),aggregate($count as Groups))",
    "compute(Rating mul 2 as Twice)/aggregate(Twice with sum as Total)",
    "compute(year(Founded) as Y)/groupby((Y),aggregate($count as Rows))",
    "groupby((Abc),aggregate(Rating with sum as Total))/compute(Total mul 2 as Doubled)",
}) {
    var url = "BusinessPartners?$apply=" + query;
    try
    {
        var uri = new Uri($"{root}/BusinessPartners?$apply={Uri.EscapeDataString(query)}");
        var parser = new Microsoft.OData.UriParser.ODataUriParser(model, new Uri(root + "/"), uri);
        var parsed = parser.ParseApply();
        var r = await Send(HttpMethod.Get, uri.ToString());
        r.Ensure(url);
        var doc = System.Text.Json.JsonDocument.Parse(r.Body);
        var rows = doc.RootElement.GetProperty("value").GetArrayLength();
        Report($"{url} ({parsed.Transformations.Count()} steps, {rows} rows)");
    }
    catch (Exception e) { Report(url, e); }
}

// Errors
try
{
    var r = await Send(HttpMethod.Get, "Nope");
    using var mr = new ODataMessageReader((IODataResponseMessage)r, Settings(), model);
    var err = mr.ReadError();
    Report($"error payload reads as ODataError ({err.Code})");
}
catch (Exception e) { Report("error payload", e); }

Console.WriteLine($"\n{passed} passed, {failed} failed");
Environment.Exit(failed == 0 ? 0 : 1);

class Response : IODataResponseMessage
{
    private readonly HttpResponseMessage res;
    public byte[] Body { get; }
    public Response(HttpResponseMessage r, byte[] body) { res = r; Body = body; }
    public IEnumerable<KeyValuePair<string, string>> Headers =>
        res.Headers.Concat(res.Content.Headers).Select(h => new KeyValuePair<string, string>(h.Key, string.Join(",", h.Value)));
    public int StatusCode { get => (int)res.StatusCode; set { } }
    public string GetHeader(string name) => Headers.FirstOrDefault(h => string.Equals(h.Key, name, StringComparison.OrdinalIgnoreCase)).Value;
    public void SetHeader(string name, string value) { }
    public Stream GetStream() => new MemoryStream(Body);
    public void Ensure(string name, int expected = 200)
    {
        if (StatusCode != expected) throw new Exception($"{name}: HTTP {StatusCode}: {Encoding.UTF8.GetString(Body)}");
    }
}

class RequestMessage : IODataRequestMessage
{
    private readonly Stream stream;
    private readonly Dictionary<string, string> headers = new();
    public RequestMessage(Stream s) { stream = s; }
    public IEnumerable<KeyValuePair<string, string>> Headers => headers;
    public Uri Url { get; set; } = new("http://unused/");
    public string Method { get; set; } = "POST";
    public string GetHeader(string name) => headers.TryGetValue(name, out var v) ? v : null;
    public void SetHeader(string name, string value) => headers[name] = value;
    public Stream GetStream() => stream;
}
