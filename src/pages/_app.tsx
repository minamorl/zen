import type { AppType } from "next/app";
import { trpc } from "../utils/trpc";
import "../styles/style.css";
import { PersonaProvider } from "@/context/personaContext";
import { ThemeProvider } from "next-themes";

import { Header } from "../header/Header";
import { FaGithub } from "react-icons/fa";
import { ConsoleProvider } from "@/context/consoleContext";
import Head from "next/head";
import Link from "next/link";

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
          <footer className="bg-black w-full text-white p-4">
            <div className="container mx-auto flex justify-center items-center">
              <Link href="https://github.com/minamorl/zen" legacyBehavior>
                <a
                  target="_blank"
                  rel="noopener noreferrer"
                  className="hover:text-gray-300 flex items-center"
                >
                  <FaGithub size={24} />
                </a>
              </Link>
            </div>
          </footer>
        </PersonaProvider>
      </ConsoleProvider>
    </ThemeProvider>
  );
};
export default trpc.withTRPC(MyApp);
