import type { NextConfig } from "next";

const HSTS = "max-age=63072000; includeSubDomains";

const nextConfig: NextConfig = {
  reactStrictMode: true,
  async headers() {
    return [
      {
        source: "/:path*",
        headers: [
          // Browsers ignore HSTS over plain http, so local development is
          // unaffected; HTTPS deployments get it on every response.
          { key: "Strict-Transport-Security", value: HSTS },
          { key: "X-Content-Type-Options", value: "nosniff" },
        ],
      },
    ];
  },
};

export default nextConfig;
