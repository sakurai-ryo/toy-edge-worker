// サブリクエスト fetch のデモ: 外部 URL を取得して加工して返す。
// await 中（ネットワーク待ち）は CPU 予算を消費しない。
export default {
  async fetch(request, env) {
    const target = env.TARGET ?? "https://example.com/";
    const upstream = await fetch(target);
    const text = await upstream.text();
    return Response.json({
      target,
      upstreamStatus: upstream.status,
      contentType: upstream.headers.get("content-type"),
      bytes: text.length,
      head: text.slice(0, 120),
    });
  },
};
