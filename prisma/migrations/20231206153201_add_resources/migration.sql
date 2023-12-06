-- CreateTable
CREATE TABLE "PostResource" (
    "id" TEXT NOT NULL,
    "postId" TEXT NOT NULL,
    "imagePath" TEXT NOT NULL,
    "createdAt" TIMESTAMP(3) NOT NULL DEFAULT CURRENT_TIMESTAMP,

    CONSTRAINT "PostResource_pkey" PRIMARY KEY ("id")
);

-- CreateTable
CREATE TABLE "PersonaResource" (
    "id" TEXT NOT NULL,
    "personaId" TEXT NOT NULL,
    "imagePath" TEXT NOT NULL,
    "createdAt" TIMESTAMP(3) NOT NULL DEFAULT CURRENT_TIMESTAMP,

    CONSTRAINT "PersonaResource_pkey" PRIMARY KEY ("id")
);

-- CreateIndex
CREATE UNIQUE INDEX "PersonaResource_personaId_key" ON "PersonaResource"("personaId");

-- AddForeignKey
ALTER TABLE "PostResource" ADD CONSTRAINT "PostResource_postId_fkey" FOREIGN KEY ("postId") REFERENCES "Post"("id") ON DELETE RESTRICT ON UPDATE CASCADE;

-- AddForeignKey
ALTER TABLE "PersonaResource" ADD CONSTRAINT "PersonaResource_personaId_fkey" FOREIGN KEY ("personaId") REFERENCES "Persona"("id") ON DELETE RESTRICT ON UPDATE CASCADE;
