import { useForm, SubmitHandler } from "react-hook-form";
import { trpc } from '../utils/trpc';


type Inputs = {
  email: string
  password: string
};
export default function SignInPage() {
  const {register, handleSubmit} = useForm<Inputs>()
  const { mutate } = trpc.signIn.useMutation()
  const onSubmit: SubmitHandler<Inputs> = (data) => {
    mutate({
      email: data.email,
      password: data.password
    })
  };
  return (
    <form onSubmit={handleSubmit(onSubmit)}>
      <input {...register("email")} />
      <input {...register("password")} />
      <input type="submit" />
    </form>
  );

  
}
