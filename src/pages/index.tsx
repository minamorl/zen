import { useEffect, useState } from 'react';
import { trpc } from '../utils/trpc';
import { on } from 'events';
import { SubmitHandler, useForm } from 'react-hook-form';

type Inputs = {
  raw_text: string
};


export default function IndexPage() {
  const {data, isLoading, mutate} = trpc.createPost.useMutation()
  const [key, setKey] = useState('invalid')
  const {handleSubmit, register} = useForm<Inputs>()
  const {data: posts, refetch, isRefetching} = trpc.getAllPosts.useQuery({})

  const onSubmit: SubmitHandler<Inputs> = async (inputs) => {
    mutate({
      raw_text: inputs.raw_text
    })
    setKey(inputs.raw_text)
    setTimeout(() => refetch(), 100)
  }


  if (!posts) {
    return <div>Loading...</div>;
  }
  return (
    <div>
      <form onSubmit={handleSubmit(onSubmit)}>
        <input
          {...register("raw_text")}
        />
        <input type="submit" value="submit" />
      </form>
      <div>
        <ul>
          { posts.map(v => <li key={v.id}>{v.raw_text}</li>) }
          { isRefetching && <li key={key}>{key}</li>}
        </ul>
      </div>
    </div>
  );
}
