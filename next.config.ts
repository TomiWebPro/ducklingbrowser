import type { NextConfig } from "next";

const nextConfig: NextConfig = {
  reactStrictMode: true,
  output: "export",
  images: {
    unoptimized: true,
  },
  // Production builds (`next build`) emit into dist/ for Tauri to embed.
  // The dev server (`next dev`) uses .next/ instead: it constantly rewrites
  // caches, logs, and traces there, and keeping that churn out of dist/
  // prevents needless Rust rebuilds (tauri-build watches frontendDist).
  distDir: process.env.NODE_ENV === "production" ? "dist" : ".next",
  compiler: {
    removeConsole: process.env.NODE_ENV === "production",
  },
};

export default nextConfig;
