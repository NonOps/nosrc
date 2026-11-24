export function handle(requestJson) {
  const request = JSON.parse(requestJson);
  if (request.method === "GET" && request.path === "/search") {
    return JSON.stringify({
      status: 200,
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ term: request.query.q || "" })
    });
  }
  return JSON.stringify({ status: 404, headers: {}, body: "not found" });
}
