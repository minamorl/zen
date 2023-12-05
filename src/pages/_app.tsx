import type { AppType } from "next/app";
import { trpc } from "../utils/trpc";
import "../styles/style.css";
import { PersonaProvider } from "@/context/personaContext";
import { ThemeProvider } from "next-themes";

import { Header } from "../header/Header";

const MyApp: AppType = ({ Component, pageProps }) => {
  return (
    <ThemeProvider defaultTheme="dark">
      <PersonaProvider>
        <Header />
        <Component {...pageProps} />
      </PersonaProvider>
    </ThemeProvider>
  );
};
export default trpc.withTRPC(MyApp);
