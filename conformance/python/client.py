"""Drives the service with python-odata, which builds its classes from $metadata."""
import os
import sys

from odata import ODataService

root = os.environ.get("ODATA_ROOT", "http://host.docker.internal:3000/odata").rstrip("/") + "/"
passed = failed = 0


def check(name, run):
    global passed, failed
    try:
        detail = run()
        passed += 1
        print(f"ok    {name}" + (f" -> {detail}" if detail is not None else ""))
    except Exception as e:
        failed += 1
        print(f"FAIL  {name}\n      {type(e).__name__}: {str(e)[:220]}")


service = ODataService(root, reflect_entities=True)
Partner = service.entities["BusinessPartners"]
Role = service.entities["Roles"]
query = lambda: service.query(Partner)

check("reflection found the entity types", lambda: len(service.entities))
check("read a set", lambda: len(query().all()))
check("$filter", lambda: [p.Name for p in query().filter(Partner.Rating > 2).order_by(Partner.Name.desc()).all()])
check("string filter", lambda: query().filter(Partner.Name == "Fourth").first().Id)
check("$select", lambda: query().select(Partner.Id, Partner.Name).limit(1).all()[0]["Name"])
check("$top and $skip", lambda: len(query().limit(2).offset(1).all()))
check("key lookup", lambda: query().get("1").Name)


def create():
    role = Role()
    role.PartnerId = "4"
    role.Code = 95
    role.Label = "python"
    service.save(role)
    return role.Label


def update():
    p = query().get("4")
    p.Name = "From Python"
    service.save(p)
    return query().get("4").Name


def delete():
    role = service.query(Role).filter(Role.Code == 95).first()
    service.delete(role)
    return len(service.query(Role).filter(Role.Code == 95).all())


check("create", create)
check("update", update)
check("delete", delete)
print(f"\n{passed} passed, {failed} failed")
sys.exit(1 if failed else 0)
