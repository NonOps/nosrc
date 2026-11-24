export function handle(requestJson) {
  const request = JSON.parse(requestJson);
  if (request.method === "POST" && request.path === "/normalize") {
    const input = JSON.parse(request.body || "{}");
    return JSON.stringify({
      status: 200,
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ name: String(input.name || "").trim().toLowerCase() })
    });
  }
  return JSON.stringify({ status: 404, headers: {}, body: "not found" });
}
