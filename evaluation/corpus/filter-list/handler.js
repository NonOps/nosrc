export function handle(requestJson) {
  const request = JSON.parse(requestJson);
  if (request.method === "POST" && request.path === "/active") {
    const rows = JSON.parse(request.body || "[]");
    return JSON.stringify({
      status: 200,
      headers: { "content-type": "application/json" },
      body: JSON.stringify(rows.filter(row => row && row.active === true))
    });
  }
  return JSON.stringify({ status: 404, headers: {}, body: "not found" });
}
