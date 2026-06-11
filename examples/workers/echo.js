export default {
  async fetch(request) {
    const body = await request.text();
    const headers = Object.fromEntries(request.headers);
    return Response.json({
      name: "echo",
      method: request.method,
      url: request.url,
      headers,
      body,
    });
  },
};
