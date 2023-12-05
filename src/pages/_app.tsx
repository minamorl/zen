import type { AppType } from "next/app";
import { trpc } from "../utils/trpc";
import "../styles/style.css";
import { PersonaProvider } from "@/context/personaContext";
import { ThemeProvider } from "next-themes";

import { Header } from "../header/Header";
import { ConsoleProvider } from "@/context/consoleContext";

const MyApp: AppType = ({ Component, pageProps }) => {
  return (
    <ThemeProvider defaultTheme="dark">
      <ConsoleProvider>
        <PersonaProvider>
          <Header />
          <Component {...pageProps} />
        </PersonaProvider>
      </ConsoleProvider>
    </ThemeProvider>
  );
};
export default trpc.withTRPC(MyApp);
