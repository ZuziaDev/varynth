import type { Metadata } from "next";
import { Space_Grotesk, Inter, JetBrains_Mono } from "next/font/google";
import { sessionToken } from "../lib/api";
import { ResponsiveNav } from "./components/ResponsiveNav";
import { ScrollAnimator } from "./components/ScrollAnimator";
import { BlackHoleBackground } from "./components/BlackHoleBackground";
import "./globals.css";

const spaceGrotesk = Space_Grotesk({
  subsets: ["latin"],
  weight: ["500", "600", "700"],
  variable: "--font-space",
  display: "swap",
});

const inter = Inter({
  subsets: ["latin"],
  weight: ["400", "500", "600", "700"],
  variable: "--font-inter",
  display: "swap",
});

const jetbrainsMono = JetBrains_Mono({
  subsets: ["latin"],
  weight: ["400", "500", "600"],
  variable: "--font-jetbrains",
  display: "swap",
});

export const metadata: Metadata = {
  title: "Varynth — Autonomous Local Coding Runtime & 61-Model Gateway",
  description:
    "A sovereign coding runtime in Rust. 61 public models ready on first launch, 7-role cognitive routing, Obsidian memory vault, and 60fps Flux video synthesis.",
  icons: {
    icon: "/favicon.svg",
    shortcut: "/favicon.svg",
    apple: "/logo.svg",
  },
};

export default async function RootLayout({ children }: { children: React.ReactNode }) {
  const signedIn = (await sessionToken()) !== null;
  return (
    <html
      lang="en"
      className={`${spaceGrotesk.variable} ${inter.variable} ${jetbrainsMono.variable}`}
    >
      <body className={inter.className}>
        <BlackHoleBackground />
        <ScrollAnimator />
        <ResponsiveNav signedIn={signedIn} />
        <div className="shell page-content-layer">{children}</div>
      </body>
    </html>
  );
}
