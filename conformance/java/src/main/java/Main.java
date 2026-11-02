// Drives the service with the Apache Olingo client, the library olingo-jpa-processor-v4 sits on.
import java.net.URI;
import java.util.Iterator;
import java.util.LinkedHashMap;
import java.util.List;
import java.util.Map;
import org.apache.olingo.client.api.ODataClient;
import org.apache.olingo.client.api.communication.ODataClientErrorException;
import org.apache.olingo.client.api.communication.request.batch.BatchManager;
import org.apache.olingo.client.api.communication.request.batch.ODataBatchResponseItem;
import org.apache.olingo.client.api.communication.request.batch.ODataChangeset;
import org.apache.olingo.client.api.communication.request.cud.ODataDeleteRequest;
import org.apache.olingo.client.api.communication.request.cud.ODataEntityCreateRequest;
import org.apache.olingo.client.api.communication.request.cud.ODataEntityUpdateRequest;
import org.apache.olingo.client.api.communication.request.cud.UpdateType;
import org.apache.olingo.client.api.communication.request.retrieve.ODataEntityRequest;
import org.apache.olingo.client.api.communication.response.ODataBatchResponse;
import org.apache.olingo.client.api.communication.response.ODataResponse;
import org.apache.olingo.client.api.domain.ClientDelta;
import org.apache.olingo.client.api.domain.ClientEntity;
import org.apache.olingo.client.api.domain.ClientEntitySet;
import org.apache.olingo.client.api.domain.ClientInvokeResult;
import org.apache.olingo.client.api.domain.ClientPrimitiveValue;
import org.apache.olingo.client.api.domain.ClientProperty;
import org.apache.olingo.client.api.domain.ClientServiceDocument;
import org.apache.olingo.client.api.domain.ClientValue;
import org.apache.olingo.client.api.uri.URIBuilder;
import org.apache.olingo.client.core.ODataClientFactory;
import org.apache.olingo.commons.api.edm.Edm;
import org.apache.olingo.commons.api.edm.EdmEntityType;
import org.apache.olingo.commons.api.edm.FullQualifiedName;
import org.apache.olingo.commons.api.format.ContentType;

public class Main {
  static final String ROOT = System.getenv().getOrDefault("ODATA_ROOT", "http://host.docker.internal:3000/odata");
  static final ODataClient client = ODataClientFactory.getClient();
  static int passed, failed;

  interface Check { String run() throws Exception; }

  static void check(String name, Check c) {
    try {
      String detail = c.run();
      passed++;
      System.out.println("ok    " + name + (detail == null ? "" : " -> " + detail));
    } catch (Throwable t) {
      failed++;
      System.out.println("FAIL  " + name + "\n      " + t.getClass().getSimpleName() + ": " + String.valueOf(t.getMessage()).split("\n")[0]);
    }
  }

  static void expect(boolean ok, String message) { if (!ok) throw new AssertionError(message); }

  static URIBuilder set(String name) { return client.newURIBuilder(ROOT).appendEntitySetSegment(name); }

  static ClientEntitySet feed(URI uri) { return client.getRetrieveRequestFactory().getEntitySetRequest(uri).execute().getBody(); }

  static ClientEntity entity(URI uri) { return client.getRetrieveRequestFactory().getEntityRequest(uri).execute().getBody(); }

  static String text(ClientEntity e, String name) {
    ClientProperty p = e.getProperty(name);
    return p == null || p.hasNullValue() ? null : p.getValue().toString();
  }

  static ClientProperty prim(String name, ClientPrimitiveValue v) { return client.getObjectFactory().newPrimitiveProperty(name, v); }

  static ClientPrimitiveValue.Builder pv() { return client.getObjectFactory().newPrimitiveValueBuilder(); }

  static ClientEntity role(String partner, int code, String label) {
    ClientEntity e = client.getObjectFactory().newEntity(new FullQualifiedName("Test.Role"));
    e.getProperties().add(prim("PartnerId", pv().buildString(partner)));
    e.getProperties().add(prim("Code", pv().buildInt32(code)));
    e.getProperties().add(prim("Label", pv().buildString(label)));
    return e;
  }

  static URI roleUri(String partner, int code) {
    Map<String, Object> key = new LinkedHashMap<>();
    key.put("PartnerId", partner);
    key.put("Code", code);
    return set("Roles").appendKeySegment(key).build();
  }

  public static void main(String[] args) {
    check("metadata parses to an Edm", () -> {
      Edm edm = client.getRetrieveRequestFactory().getMetadataRequest(ROOT).execute().getBody();
      EdmEntityType t = edm.getEntityType(new FullQualifiedName("Test.BusinessPartner"));
      expect(t != null, "no BusinessPartner type");
      expect(t.getKeyPredicateNames().contains("Id"), "no key");
      expect(edm.getEntityContainer().getEntitySet("BusinessPartners").getRelatedBindingTarget("Roles") != null, "no binding");
      EdmEntityType person = edm.getEntityType(new FullQualifiedName("Test.Person"));
      expect(person.getBaseType().getName().equals("BusinessPartner"), "no base type");
      return edm.getSchemas().get(0).getEntityTypes().size() + " entity types";
    });
    check("service document", () -> {
      ClientServiceDocument d = client.getRetrieveRequestFactory().getServiceDocumentRequest(ROOT).execute().getBody();
      expect(d.getEntitySetURI("BusinessPartners") != null, "no set");
      return d.getEntitySets().size() + " sets";
    });
    check("read a set", () -> {
      ClientEntitySet s = feed(set("BusinessPartners").build());
      expect(s.getEntities().size() == 4, "size " + s.getEntities().size());
      return "4 entities";
    });
    check("read an entity by key", () -> {
      ClientEntity e = entity(set("BusinessPartners").appendKeySegment("1").build());
      expect("First Org".equals(text(e, "Name")), text(e, "Name"));
      return "ETag " + e.getETag();
    });
    // Olingo does not double quotes in keys, so the test does it.
    check("string key with a quote", () -> {
      try { entity(set("BusinessPartners").appendKeySegment("O''Neil").build()); }
      catch (ODataClientErrorException e) { expect(e.getStatusLine().getStatusCode() == 404, e.getMessage()); return "404"; }
      throw new AssertionError("expected 404");
    });
    check("composite key", () -> {
      expect("Seller".equals(text(entity(roleUri("1", 2)), "Label")), "label");
      return null;
    });
    check("$filter from FilterFactory", () -> {
      var f = client.getFilterFactory();
      var filter = f.and(f.gt("Rating", 2), f.eq("Active", true));
      ClientEntitySet s = feed(set("BusinessPartners").filter(filter).build());
      expect(s.getEntities().size() == 1, "size " + s.getEntities().size());
      return filter.build();
    });
    for (String expr : List.of(
        "contains(Name,'Org')", "Address/City eq 'Berlin'", "Roles/any(r:r/Code eq 3)",
        "Rating in (1,5)", "Founded gt 2000-01-01", "CreatedAt lt 2024-06-01T00:00:00+02:00",
        "Abc eq Test.Abc'A'", "Name eq 'Third O''Neil'", "Revenue gt 100.5",
        "ExternalId eq 01234567-89ab-cdef-0123-456789abcdef")) {
      check("$filter=" + expr, () -> feed(set("BusinessPartners").filter(expr).build()).getEntities().size() + " rows");
    }
    check("$select, $expand, $orderby, $top, $skip, $count", () -> {
      URI uri = set("BusinessPartners").select("Name").expandWithOptions("Roles", Map.of(
          org.apache.olingo.client.api.uri.QueryOption.TOP, 1)).orderBy("Name desc").top(2).skip(1).count(true).build();
      ClientEntitySet s = feed(uri);
      expect(s.getCount() == 4, "count " + s.getCount());
      expect(s.getEntities().size() == 2, "size");
      expect(s.getEntities().get(0).getNavigationLink("Roles") != null, "no inline Roles");
      return uri.getRawQuery();
    });
    check("derived type from @odata.type", () -> {
      ClientEntity e = entity(set("BusinessPartners").appendKeySegment("3").build());
      expect(e.getTypeName().getName().equals("Person"), String.valueOf(e.getTypeName()));
      return e.getTypeName().toString();
    });
    check("navigation path", () -> feed(set("BusinessPartners").appendKeySegment("1").appendNavigationSegment("Roles").build()).getEntities().size() + " roles");
    check("$count segment", () -> client.getRetrieveRequestFactory()
        .getValueRequest(set("BusinessPartners").count().build()).execute().getBody().toString());
    check("$value", () -> client.getRetrieveRequestFactory().getValueRequest(
        set("BusinessPartners").appendKeySegment("1").appendPropertySegment("Name").appendValueSegment().build())
        .execute().getBody().toString());
    check("property read", () -> client.getRetrieveRequestFactory().getPropertyRequest(
        set("BusinessPartners").appendKeySegment("1").appendPropertySegment("Address").build())
        .execute().getBody().getName());
    check("create", () -> {
      ODataEntityCreateRequest<ClientEntity> r = client.getCUDRequestFactory()
          .getEntityCreateRequest(set("Roles").build(), role("4", 70, "created"));
      var res = r.execute();
      expect(res.getStatusCode() == 201, "status " + res.getStatusCode());
      expect("created".equals(text(res.getBody(), "Label")), "label");
      return res.getBody().getId() == null ? "no id" : res.getBody().getId().toString();
    });
    check("update with PATCH", () -> {
      ClientEntity patch = client.getObjectFactory().newEntity(new FullQualifiedName("Test.Role"));
      patch.getProperties().add(prim("Label", pv().buildString("patched")));
      ODataEntityUpdateRequest<ClientEntity> r = client.getCUDRequestFactory()
          .getEntityUpdateRequest(roleUri("4", 70), UpdateType.PATCH, patch);
      expect(r.execute().getStatusCode() == 204, "status");
      expect("patched".equals(text(entity(roleUri("4", 70)), "Label")), "not patched");
      return null;
    });
    check("If-Match with the ETag the client read", () -> {
      URI uri = set("BusinessPartners").appendKeySegment("4").build();
      ODataEntityRequest<ClientEntity> read = client.getRetrieveRequestFactory().getEntityRequest(uri);
      var body = read.execute().getBody();
      String tag = body.getETag();
      ClientEntity patch = client.getObjectFactory().newEntity(new FullQualifiedName("Test.BusinessPartner"));
      patch.getProperties().add(prim("Name", pv().buildString("Locked")));
      var ok = client.getCUDRequestFactory().getEntityUpdateRequest(uri, UpdateType.PATCH, patch);
      ok.setIfMatch(tag);
      expect(ok.execute().getStatusCode() == 204, "status");
      var stale = client.getCUDRequestFactory().getEntityUpdateRequest(uri, UpdateType.PATCH, patch);
      stale.setIfMatch(tag);
      try { stale.execute(); }
      catch (ODataClientErrorException e) { expect(e.getStatusLine().getStatusCode() == 412, e.getMessage()); return "stale tag got 412"; }
      throw new AssertionError("expected 412");
    });
    check("upsert with PATCH", () -> {
      ClientEntity patch = client.getObjectFactory().newEntity(new FullQualifiedName("Test.Role"));
      patch.getProperties().add(prim("Label", pv().buildString("upserted")));
      var res = client.getCUDRequestFactory().getEntityUpdateRequest(roleUri("4", 72), UpdateType.PATCH, patch).execute();
      expect(res.getStatusCode() == 201, "status " + res.getStatusCode());
      expect("upserted".equals(text(entity(roleUri("4", 72)), "Label")), "not stored");
      var second = client.getCUDRequestFactory().getEntityUpdateRequest(roleUri("4", 72), UpdateType.PATCH, patch).execute();
      expect(second.getStatusCode() == 204, "second status " + second.getStatusCode());
      return "201, then 204";
    });
    check("deep update with an inline set", () -> {
      ClientEntity parent = client.getObjectFactory().newEntity(new FullQualifiedName("Test.BusinessPartner"));
      parent.getProperties().add(prim("Name", pv().buildString("Deep from Olingo")));
      var roles = client.getObjectFactory().newEntitySet();
      ClientEntity child = client.getObjectFactory().newEntity(new FullQualifiedName("Test.Role"));
      child.getProperties().add(prim("Code", pv().buildInt32(6)));
      child.getProperties().add(prim("Label", pv().buildString("nested")));
      roles.getEntities().add(child);
      parent.addLink(client.getObjectFactory().newDeepInsertEntitySet("Roles", roles));
      URI uri = set("BusinessPartners").appendKeySegment("2").build();
      var res = client.getCUDRequestFactory().getEntityUpdateRequest(uri, UpdateType.PATCH, parent).execute();
      expect(res.getStatusCode() == 204, "status " + res.getStatusCode());
      var now = feed(set("BusinessPartners").appendKeySegment("2").appendNavigationSegment("Roles").build());
      expect(now.getEntities().size() == 1 && "nested".equals(text(now.getEntities().get(0), "Label")), "roles");
      return "1 role replaced";
    });
    check("delete", () -> {
      ODataDeleteRequest r = client.getCUDRequestFactory().getDeleteRequest(roleUri("4", 70));
      expect(r.execute().getStatusCode() == 204, "status");
      return null;
    });
    check("function import", () -> {
      Map<String, ClientValue> params = new LinkedHashMap<>();
      params.put("a", pv().buildInt32(2));
      params.put("b", pv().buildInt32(3));
      var r = client.getInvokeRequestFactory().getFunctionInvokeRequest(
          client.newURIBuilder(ROOT).appendOperationCallSegment("Add").build(), ClientProperty.class, params);
      var v = r.execute().getBody();
      expect("5".equals(v.getValue().toString()), v.getValue().toString());
      return "5";
    });
    check("action import", () -> {
      Map<String, ClientValue> params = new LinkedHashMap<>();
      params.put("a", pv().buildInt32(4));
      params.put("b", pv().buildInt32(5));
      var r = client.getInvokeRequestFactory().getActionInvokeRequest(
          client.newURIBuilder(ROOT).appendOperationCallSegment("Sum").build(), ClientProperty.class, params);
      var v = r.execute().getBody();
      expect("9".equals(v.getValue().toString()), v.getValue().toString());
      return "9";
    });
    check("multipart $batch with a change set", () -> {
      var batch = client.getBatchRequestFactory().getBatchRequest(ROOT);
      BatchManager m = batch.payloadManager();
      m.addRequest(client.getRetrieveRequestFactory().getEntityRequest(set("BusinessPartners").appendKeySegment("1").build()));
      ODataChangeset cs = m.addChangeset();
      cs.addRequest(client.getCUDRequestFactory().getEntityCreateRequest(set("Roles").build(), role("4", 71, "batched")));
      ODataBatchResponse res = m.getResponse();
      expect(res.getStatusCode() == 200, "status " + res.getStatusCode());
      Iterator<ODataBatchResponseItem> items = res.getBody();
      StringBuilder seen = new StringBuilder();
      while (items.hasNext()) {
        ODataBatchResponseItem item = items.next();
        while (item.hasNext()) { ODataResponse r = item.next(); seen.append(r.getStatusCode()).append(' '); }
      }
      expect(seen.toString().trim().equals("200 201"), seen.toString());
      expect("batched".equals(text(entity(roleUri("4", 71)), "Label")), "not stored");
      return seen.toString().trim();
    });
    check("error payload", () -> {
      try { feed(set("Nope").build()); }
      catch (ODataClientErrorException e) {
        expect(e.getStatusLine().getStatusCode() == 404, "status");
        expect(e.getODataError() != null && "404".equals(e.getODataError().getCode()), "no error body");
        return e.getODataError().getMessage();
      }
      throw new AssertionError("expected an error");
    });
    check("odata.metadata=full", () -> {
      var r = client.getRetrieveRequestFactory().getEntityRequest(set("BusinessPartners").appendKeySegment("1").build());
      r.setAccept(ContentType.JSON_FULL_METADATA.toContentTypeString());
      ClientEntity e = r.execute().getBody();
      expect(e.getEditLink() != null, "no edit link");
      expect(e.getNavigationLink("Roles") != null, "no navigation link");
      return e.getEditLink().toString();
    });
    check("change tracking", () -> {
      var first = client.getRetrieveRequestFactory().getEntitySetRequest(set("Notes").build());
      first.setPrefer(client.newPreferences().trackChanges());
      var read = first.execute();
      expect("odata.track-changes".equals(read.getHeader("Preference-Applied").iterator().next()), "not applied");
      URI link = read.getBody().getDeltaLink();
      expect(link != null, "no delta link");
      client.getCUDRequestFactory().getDeleteRequest(set("Notes").appendKeySegment(2).build()).execute();
      ClientDelta delta = client.getRetrieveRequestFactory().getDeltaRequest(link).execute().getBody();
      expect(delta.getDeletedEntities().size() == 1, delta.getDeletedEntities().size() + " deleted");
      expect(delta.getDeltaLink() != null, "no next link");
      return delta.getDeletedEntities().get(0).getId() + " " + delta.getDeletedEntities().get(0).getReason();
    });
    System.out.println("\n" + passed + " passed, " + failed + " failed");
    System.exit(failed == 0 ? 0 : 1);
  }
}
