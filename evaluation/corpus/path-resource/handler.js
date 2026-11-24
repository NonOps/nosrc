export function handle(requestJson) {
  const request = JSON.parse(requestJson);
  const match = request.path.match(/^\/items\/([^/]+)$/);
  if (request.method === "GET" && match) {
    return JSON.stringify({
      status: 200,
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ id: match[1] })
    });
  }
  return JSON.stringify({ status: 404, headers: {}, body: "not found" });
}
