import type { AppType } from "next/app";
import { trpc } from "../utils/trpc";
import "../styles/style.css";
import { PersonaProvider } from "@/context/personaContext";
import { ThemeProvider } from "next-themes";

import { Header } from "../header/Header";
import { ConsoleProvider } from "@/context/consoleContext";
import Head from "next/head";

const MyApp: AppType = ({ Component, pageProps }) => {
  return (
    <ThemeProvider defaultTheme="dark">
      <Head>
        <meta
          name="viewport"
          content="width=device-width, initial-scale=1.0, maximum-scale=1.0,user-scalable=0"
        />
      </Head>
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
