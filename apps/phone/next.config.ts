import type { NextConfig } from "next";

const config: NextConfig = {
  output: "export",
  reactStrictMode: true,
  images: { unoptimized: true },
  transpilePackages: ["@ssc/protocol"],
  devIndicators: false,
};

export default config;
