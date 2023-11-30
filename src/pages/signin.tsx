import { useForm, SubmitHandler } from "react-hook-form";
import { trpc } from '../utils/trpc';
import { useEffect, useState } from 'react'


type Inputs = {
  email: string
  password: string
};
export default function SignInPage() {
  const {register, handleSubmit} = useForm<Inputs>()
  const { data , mutate } = trpc.signIn.useMutation()
  const onSubmit: SubmitHandler<Inputs> = (data) => {
    mutate({
      email: data.email,
      password: data.password
    })
  };


  const {data: personas} = trpc.getPersonas.useQuery()
  console.log(data)
    

  return (
    <div>
      <form onSubmit={handleSubmit(onSubmit)}>
        <input className='peer w-full h-full bg-transparent text-blue-gray-700 font-sans font-normal outline outline-0 focus:outline-0 disabled:bg-blue-gray-50 disabled:border-0 transition-all placeholder-shown:border placeholder-shown:border-blue-gray-200 placeholder-shown:border-t-blue-gray-200 border focus:border-2 border-t-transparent focus:border-t-transparent text-sm px-3 py-2.5 rounded-[7px] border-blue-gray-200 focus:border-gray-900' {...register("email")} />
        <input className='peer w-full h-full bg-transparent text-blue-gray-700 font-sans font-normal outline outline-0 focus:outline-0 disabled:bg-blue-gray-50 disabled:border-0 transition-all placeholder-shown:border placeholder-shown:border-blue-gray-200 placeholder-shown:border-t-blue-gray-200 border focus:border-2 border-t-transparent focus:border-t-transparent text-sm px-3 py-2.5 rounded-[7px] border-blue-gray-200 focus:border-gray-900' {...register("password")} />
        <input className='bg-blue-500 hover:bg-blue-700 text-white font-bold py-2 px-4 rounded-full' type="submit" value="Sign In" />
      </form>
      <ul>
        { personas?.user && 'You are logged in as ' + personas.user.email}
        { personas?.personas && personas.personas.map(v => <li key={v.id}>{v.name}</li>) }
      </ul>
    </div>
  );

  
}
