export function handle(requestJson) {
  const request = JSON.parse(requestJson);
  if (request.method === "GET" && request.path === "/healthz") {
    return JSON.stringify({
      status: 200,
      headers: { "content-type": "text/plain" },
      body: "healthy"
    });
  }
  return JSON.stringify({ status: 404, headers: {}, body: "not found" });
}
