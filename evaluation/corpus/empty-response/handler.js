export function handle(requestJson) {
  const request = JSON.parse(requestJson);
  if (request.method === "GET" && request.path === "/empty") {
    return JSON.stringify({ status: 204, headers: {}, body: "" });
  }
  return JSON.stringify({ status: 404, headers: {}, body: "not found" });
}
