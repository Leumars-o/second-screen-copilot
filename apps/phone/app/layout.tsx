import type { Metadata, Viewport } from "next";
import type { ReactNode } from "react";
import { RegisterSW } from "../src/ui/RegisterSW";
import "./globals.css";

export const metadata: Metadata = {
  title: "Second Screen Copilot",
  description: "Live cues from your PC copilot.",
  manifest: "/manifest.webmanifest",
  icons: { icon: "/icon-192.png", apple: "/icon-192.png" },
  appleWebApp: { capable: true, statusBarStyle: "black-translucent", title: "Copilot" },
};

export const viewport: Viewport = {
  width: "device-width",
  initialScale: 1,
  viewportFit: "cover",
  themeColor: "#0b0d10",
  colorScheme: "dark",
};

export default function RootLayout({ children }: { children: ReactNode }) {
  return (
    <html lang="en">
      <body>
        {children}
        <RegisterSW />
      </body>
    </html>
  );
}
