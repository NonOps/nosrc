const items = [];

export function handle(requestJson) {
  const request = JSON.parse(requestJson);
  if (request.method === "POST" && request.path === "/items") {
    const input = JSON.parse(request.body || "{}");
    const id = String(items.length + 1);
    const item = { id, name: String(input.name || "") };
    items.push(item);
    return JSON.stringify({
      status: 201,
      headers: { "content-type": "application/json", location: `/items/${id}` },
      body: JSON.stringify({ id: item.id, name: item.name })
    });
  }
  const match = request.path.match(/^\/items\/([^/]+)$/);
  if (request.method === "GET" && match) {
    const item = items.find(candidate => candidate.id === match[1]);
    if (!item) return JSON.stringify({ status: 404, headers: {}, body: "not found" });
    return JSON.stringify({
      status: 200,
      headers: { "content-type": "application/json" },
      body: JSON.stringify(item)
    });
  }
  return JSON.stringify({ status: 404, headers: {}, body: "not found" });
}
