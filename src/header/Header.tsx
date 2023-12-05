import Link from "next/link";
import { ConsoleUI } from "@/console";
import { useConsole } from "../context/consoleContext";
const SignInButton = () => {
  return (
    <div className="flex-none self-start w-24 text-white bg-blue-600">
      <Link href={"/signin"}>Sign In</Link>
    </div>
  );
};

export const Header = () => {
  const messages = useConsole();
  return (
    <header className="w-full m-4 text-2xl sticky top-0">
      <div className="flex">
        <h1 className="flex-none mr-8 w-6 font-extrabold display-block">
          <Link href="/">zen</Link>
        </h1>
        <div className="flex-1"></div>
        <SignInButton />
      </div>
      <div className="text-white ">
        <ConsoleUI messages={messages} />
      </div>
    </header>
  );
};
