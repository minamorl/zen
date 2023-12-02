import Link from "next/link";
const SignInButton = () => {
  return (
    <div className="flex-none self-start w-24 text-white bg-blue-600">
      <Link href={"/signin"}>Sign In</Link>
    </div>
  );
};

export const Header = () => {
  return (
    <header className="w-full m-4 flex text-2xl sticky top-0">
      <h1 className="flex-none mr-8 w-6 font-extrabold display-block">
        <Link href="/">zen</Link>
      </h1>
      <div className="flex-1">A progressive social media platform</div>
      <SignInButton />
    </header>
  );
};
