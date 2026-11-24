export function handle(requestJson) {
  const request = JSON.parse(requestJson);
  if (request.method === "POST" && request.path === "/parse") {
    try {
      JSON.parse(request.body || "");
      return JSON.stringify({ status: 200, headers: {}, body: "parsed" });
    } catch (_error) {
      return JSON.stringify({ status: 400, headers: { "content-type": "text/plain" }, body: "invalid JSON" });
    }
  }
  return JSON.stringify({ status: 404, headers: {}, body: "not found" });
}
