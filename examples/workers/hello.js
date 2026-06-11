export default {
  async fetch(request, env, ctx) {
    console.log(`${request.method} ${request.url}`);
    // microtask と pending op（timer）の両方を通るデモ
    await new Promise((resolve) => setTimeout(resolve, 10));
    return Response.json({
      hello: "world",
      url: request.url,
      greeting: env.GREETING ?? null,
    });
  },
};
