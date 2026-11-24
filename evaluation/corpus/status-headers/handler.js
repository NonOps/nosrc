export function handle(requestJson) {
  const request = JSON.parse(requestJson);
  if (request.method === "GET" && request.path === "/jobs/queued") {
    return JSON.stringify({
      status: 202,
      headers: { "content-type": "text/plain", "x-job-state": "queued" },
      body: "queued"
    });
  }
  return JSON.stringify({ status: 404, headers: {}, body: "not found" });
}
