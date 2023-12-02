import type { AppType } from "next/app";
import { trpc } from "../utils/trpc";
import "../styles/style.css";
import { PersonaProvider } from "@/context/personaContext";

const MyApp: AppType = ({ Component, pageProps }) => {
  return (
    <PersonaProvider>
      <Component {...pageProps} />
    </PersonaProvider>
  );
};
export default trpc.withTRPC(MyApp);
