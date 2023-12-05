import { useForm } from "react-hook-form";
import { trpc } from "../utils/trpc";
import { SubmitHandler } from "react-hook-form";

type Inputs = {
  email: string;
  password: string;
};
export default function SignUpPage() {
  // react-hook-form
  //
  const { handleSubmit, register } = useForm<Inputs>();
  const { mutate, error } = trpc.signUp.useMutation();
  const onSubmit: SubmitHandler<Inputs> = async (inputs) => {
    const r = mutate({
      email: inputs.email,
      password: inputs.password,
    });
    console.log(r);
  };
  return (
    <div>
      <form onSubmit={handleSubmit(onSubmit)}>
        <input
          {...register("email")}
          className="border border-gray-300 rounded-md"
          type="email"
          placeholder="email"
        />
        <input
          {...register("password")}
          className="border border-gray-300 rounded-md"
          type="password"
          placeholder="password"
        />
        <button type="submit">Submit</button>
        {error && <div className="text-red-500">{error.message}</div>}
      </form>
    </div>
  );
}
