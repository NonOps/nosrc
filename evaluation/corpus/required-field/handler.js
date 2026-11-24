export function handle(requestJson) {
  const request = JSON.parse(requestJson);
  if (request.method === "POST" && request.path === "/users") {
    const input = JSON.parse(request.body || "{}");
    if (typeof input.name !== "string" || !input.name.trim()) {
      return JSON.stringify({ status: 400, headers: { "content-type": "text/plain" }, body: "name is required" });
    }
    return JSON.stringify({ status: 201, headers: {}, body: "created" });
  }
  return JSON.stringify({ status: 404, headers: {}, body: "not found" });
}
